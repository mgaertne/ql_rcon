use core::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use azmq::{
    context::ZmqContextBuilder,
    message::ZmqMessage,
    socket::{
        AsyncMonitorReceiver, AsyncZmqReceiver, Monitor, MonitorFlags, MonitorSocketEvent,
        Subscriber, ZmqSocket,
    },
};
use serde_json::Value;
use tokio::{
    select,
    sync::{RwLock, mpsc::UnboundedSender},
};
use uuid::Uuid;

use crate::{CONTINUE_RUNNING, cmd_line::CommandLineOptions};

struct MonitoredSubscriber {
    subscriber: RwLock<ZmqSocket<Subscriber>>,
    monitor: RwLock<ZmqSocket<Monitor>>,
}

unsafe impl Send for MonitoredSubscriber {}
unsafe impl Sync for MonitoredSubscriber {}

impl MonitoredSubscriber {
    fn new() -> Result<Self> {
        let context = ZmqContextBuilder::new()
            .blocky(false)
            .max_sockets(10)
            .io_threads(1)
            .build()?;
        let subscriber = ZmqSocket::from_context(&context)?;
        let monitor = subscriber.monitor(
            MonitorFlags::HandshakeSucceeded
                | MonitorFlags::HandshakeFailedAuth
                | MonitorFlags::HandshakeFailedProtocol
                | MonitorFlags::HandshakeFailedNoDetail
                | MonitorFlags::MonitorStopped
                | MonitorFlags::Disconnected
                | MonitorFlags::Closed,
        )?;

        Ok(Self {
            subscriber: subscriber.into(),
            monitor: monitor.into(),
        })
    }

    async fn configure(&self, password: &str, identity: &str) -> Result<()> {
        let subscriber = self.subscriber.read().await;
        subscriber.set_plain_username(Some("stats"))?;
        if !password.is_empty() {
            subscriber.set_plain_password(Some(password))?;
        } else {
            subscriber.set_plain_password(None::<&str>)?;
        }

        let identity_str = if identity.is_empty() {
            let identity = Uuid::new_v4();
            identity.to_string().replace("-", "")
        } else {
            identity.to_string()
        };

        subscriber.set_identity(identity_str)?;

        subscriber.set_rcvtimeo(0)?;
        subscriber.set_rcvhwm(0)?;
        subscriber.set_sndtimeo(0)?;
        subscriber.set_sndhwm(0)?;

        subscriber.set_heartbeat_ivl(600_000)?;
        subscriber.set_heartbeat_timeout(600_000)?;

        subscriber.set_zap_domain("stats")?;

        Ok(())
    }

    async fn connect(&self, address: &str) -> Result<()> {
        let socket = self.subscriber.read().await;
        socket.connect(address)?;

        socket.subscribe("")?;

        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        let subscriber = self.subscriber.read().await;
        let last_endpoint = subscriber.last_endpoint()?;
        subscriber.disconnect(&last_endpoint)?;

        Ok(())
    }

    async fn recv_msg(&self) -> Option<ZmqMessage> {
        let subscriber = self.subscriber.read().await;
        subscriber.recv_msg_async().await
    }

    async fn check_monitor(&self) -> Option<MonitorSocketEvent> {
        let monitor = self.monitor.read().await;
        monitor.recv_monitor_event_async().await
    }
}

fn format_ql_json(msg: &str, args: &CommandLineOptions) -> String {
    serde_json::from_str::<Value>(msg)
        .and_then(|parsed_json| {
            if args.pretty_print {
                serde_json::to_string_pretty(&parsed_json)
            } else {
                serde_json::to_string(&parsed_json)
            }
        })
        .unwrap_or(msg.to_string())
}

static FIRST_TIME: AtomicBool = AtomicBool::new(true);

async fn check_monitor(
    monitored_dealer: &MonitoredSubscriber,
    sender: &UnboundedSender<String>,
    endpoint: &str,
) -> Result<()> {
    match monitored_dealer.check_monitor().await {
        Some(MonitorSocketEvent::HandshakeSucceeded) => {
            FIRST_TIME.store(true, Ordering::Release);
            sender.send(format!("ZMQ connected to {}.", &endpoint))?;
        }

        Some(
            event @ (MonitorSocketEvent::HandshakeFailedAuth(_)
            | MonitorSocketEvent::HandshakeFailedProtocol(_)
            | MonitorSocketEvent::HandshakeFailedNoDetail(_)
            | MonitorSocketEvent::MonitorStopped),
        ) => {
            sender.send(format!("ZMQ socket error: {event:?}"))?;
            CONTINUE_RUNNING.store(false, Ordering::Release);
        }

        Some(MonitorSocketEvent::Disconnected | MonitorSocketEvent::Closed) => {
            if FIRST_TIME.load(Ordering::Acquire) {
                FIRST_TIME.store(false, Ordering::Release);
                sender.send("Reconnecting ZMQ...".to_string())?;
            }
            if let Err(e) = monitored_dealer.connect(endpoint).await {
                sender.send(format!("error reconnecting: {e:?}."))?;
            }
        }

        Some(
            MonitorSocketEvent::Connected
            | MonitorSocketEvent::ConnectDelayed
            | MonitorSocketEvent::ConnectRetried(_),
        ) => (),

        Some(event) => {
            sender.send(format!("ZMQ socket error: {event:?}",))?;
        }

        _ => (),
    };

    Ok(())
}

pub(crate) async fn run_zmq(
    args: CommandLineOptions,
    display_sender: UnboundedSender<String>,
) -> Result<()> {
    display_sender.send(format!("ZMQ connecting to {}...", &args.host))?;

    let monitored_dealer = MonitoredSubscriber::new()?;
    monitored_dealer
        .configure(&args.password, &args.identity)
        .await?;

    monitored_dealer.connect(&args.host).await?;

    while CONTINUE_RUNNING.load(Ordering::Acquire) {
        select!(
            biased;

            Some(zmq_msg) = monitored_dealer.recv_msg() => {
                let zmq_str = zmq_msg.to_string();
                display_sender.send(format_ql_json(&zmq_str, &args))?;
            }

            Ok(()) = check_monitor(&monitored_dealer, &display_sender, &args.host) => (),

            else => ()
        );
    }

    monitored_dealer.disconnect().await?;

    drop(display_sender);

    Ok(())
}
