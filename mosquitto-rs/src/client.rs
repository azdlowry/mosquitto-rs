use crate::lowlevel::sys::{mosq_err_t, mosq_opt_t};
use crate::lowlevel::{Callbacks, MessageId, Mosq, QoS};
use crate::ReasonCode;
use crate::{ConnectionStatus, Error, PasswdCallback};
use async_channel::{bounded, unbounded, Receiver, Sender};
use std::collections::HashMap;
use std::os::raw::c_int;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

/// An event received either from the broker, or from
/// the thread that is managing the connection to the
/// broker.
#[derive(Debug, Clone)]
pub enum Event {
    /// A message was received from one of your subscriptions.
    Message(Message),
    /// The session was (re)connected.
    /// You will need to (re-)subscribe to topics of
    /// interest.
    Connected(ConnectionStatus),
    /// The session was disconnected.
    /// For unexpected disconnects, the client will
    /// automatically try to reconnect.
    Disconnected(ReasonCode),
}

struct Handler {
    connect: Mutex<Option<Sender<ConnectionStatus>>>,
    mids: Mutex<HashMap<MessageId, Sender<MessageId>>>,
    subscriber_tx: Mutex<Option<Sender<Event>>>,
    subscriber_rx: Mutex<Option<Receiver<Event>>>,
}

impl Handler {
    fn new() -> Self {
        let (tx, rx) = unbounded();
        Self {
            connect: Mutex::new(None),
            mids: Mutex::new(HashMap::new()),
            subscriber_tx: Mutex::new(Some(tx)),
            subscriber_rx: Mutex::new(Some(rx)),
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[repr(i32)]
pub enum ProtocolVersion {
    V31 = 3,
    V311 = 4,
    V5 = 5,
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self::V31
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ClientOption<'a> {
    /// Specifies the version of the MQTT protocol to be used.
    /// Defaults to ProtocolVersion::V31
    ProtocolVersion(ProtocolVersion),

    /// Value can be set between 1 and 65535 inclusive, and represents the maximum number of
    /// incoming QoS 1 and QoS 2 messages that this client wants to process at once. Defaults to
    /// 20. This option is not valid for MQTT v3.1 or v3.1.1 clients.  Note that if the
    /// MQTT_PROP_RECEIVE_MAXIMUM property is in the proplist passed to mosquitto_connect_v5(),
    /// then that property will override this option. Using this option is the recommended method
    /// however.
    ReceiveMaximum(u16),

    /// Value can be set between 1 and 65535 inclusive, and represents the maximum number of
    /// outgoing QoS 1 and QoS 2 messages that this client will attempt to have "in flight" at
    /// once. Defaults to 20.  This option is not valid for MQTT v3.1 or v3.1.1 clients.  Note that
    /// if the broker being connected to sends a MQTT_PROP_RECEIVE_MAXIMUM property that has a
    /// lower value than this option, then the broker provided value will be used.
    SendMaximum(u16),

    /// Set whether OCSP checking on TLS connections is required.
    /// The default is false for no checking
    OcspRequired(bool),

    /// Configure the client for TLS Engine support; set this to a TLS Engine ID
    /// to be used when creating TLS connections.
    TlsEngine(&'a str),

    /// Configure the client to treat the keyfile differently depending on its type.  Must be set
    /// before <mosquitto_connect>.  Set as either "pem" or "engine", to determine from where the
    /// private key for a TLS connection will be obtained. Defaults to "pem", a normal private key
    /// file.
    TlsKeyForm(&'a str),

    /// Where the TLS Engine requires the use of a password to be accessed, this option allows a
    /// hex encoded SHA1 hash of the private key password to be passed to the engine directly.
    /// Must be set before <mosquitto_connect>.
    TlsKPassSha1(&'a str),

    /// If the broker being connected to has multiple services available on a single TLS port, such
    /// as both MQTT and WebSockets, use this option to configure the ALPN option for the
    /// connection.
    TlsALPN(&'a str),
}

/// Represents a received message that matches one or
/// more of the subscription topic patterns on a client.
#[derive(Clone, Eq, PartialEq, Default)]
pub struct Message {
    /// The destination topic
    pub topic: String,
    /// The data payload bytes
    pub payload: Vec<u8>,
    /// The qos level at which the message was sent
    pub qos: QoS,
    /// Whether the message is a retained message.
    /// The broker will preserve the last retained
    /// message and send it to a subscriber at subscribe
    /// time.
    pub retain: bool,
    /// The message id
    pub mid: MessageId,
}

struct PayloadPrinter<'a>(&'a [u8]);
impl<'a> std::fmt::Debug for PayloadPrinter<'a> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        match std::str::from_utf8(&self.0) {
            Ok(payload) => payload.fmt(fmt),
            Err(_) => fmt.write_fmt(format_args!("{:02X?}", self.0)),
        }
    }
}

impl std::fmt::Debug for Message {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        fmt.debug_struct("Message")
            .field("topic", &self.topic)
            .field("payload", &PayloadPrinter(&self.payload))
            .field("qos", &self.qos)
            .field("retain", &self.retain)
            .field("mid", &self.mid)
            .finish()
    }
}

impl Handler {
    fn dispatch_event(&self, client: &mut Mosq, event: Event) {
        match self.subscriber_tx.lock().unwrap().as_ref() {
            Some(tx) => {
                if tx.try_send(event).is_err() {
                    let _ = client.disconnect();
                }
            }
            None => {
                let _ = client.disconnect();
            }
        }
    }
}

impl Callbacks for Handler {
    fn on_connect(&self, client: &mut Mosq, reason: ConnectionStatus) {
        let mut connect = self.connect.lock().unwrap();
        log::trace!("connected: {reason}");
        if let Some(connect) = connect.take() {
            if connect.try_send(reason).is_err() {
                let _ = client.disconnect();
            }
        }
        self.dispatch_event(client, Event::Connected(reason));
    }

    fn on_publish(&self, client: &mut Mosq, mid: MessageId) {
        let mut mids = self.mids.lock().unwrap();
        if let Some(tx) = mids.remove(&mid) {
            if tx.try_send(mid).is_err() {
                let _ = client.disconnect();
            }
        } else {
            let _ = client.disconnect();
        }
    }

    fn on_subscribe(&self, client: &mut Mosq, mid: MessageId, _granted_qos: &[QoS]) {
        let mut mids = self.mids.lock().unwrap();
        if let Some(tx) = mids.remove(&mid) {
            if tx.try_send(mid).is_err() {
                let _ = client.disconnect();
            }
        } else {
            let _ = client.disconnect();
        }
    }

    fn on_unsubscribe(&self, client: &mut Mosq, mid: MessageId) {
        let mut mids = self.mids.lock().unwrap();
        if let Some(tx) = mids.remove(&mid) {
            if tx.try_send(mid).is_err() {
                let _ = client.disconnect();
            }
        } else {
            let _ = client.disconnect();
        }
    }

    fn on_disconnect(&self, client: &mut Mosq, reason: ReasonCode) {
        self.mids.lock().unwrap().clear();
        self.dispatch_event(client, Event::Disconnected(reason));
        log::trace!("client disconnected with reason={reason}");
        if !reason.is_unexpected_disconnect() {
            // mosquitto won't auto-reconnect in this case,
            // so we need to signal to our consumer that we are done.
            self.subscriber_tx.lock().unwrap().take();
        }
    }

    fn on_message(
        &self,
        client: &mut Mosq,
        mid: MessageId,
        topic: String,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) {
        let m = Message {
            mid,
            topic,
            payload: payload.to_vec(),
            qos,
            retain,
        };
        self.dispatch_event(client, Event::Message(m));
    }
}

/// A high-level, asynchronous mosquitto MQTT client
#[derive(Clone)]
pub struct Client {
    mosq: Arc<Mosq<Handler>>,
}

impl Client {
    /// Create a new client instance with the specified id.
    /// If clean_session is true, instructs the broker to clean all messages
    /// and subscriptions on disconnect.  Otherwise it will preserve them.
    pub fn with_id(id: &str, clean_session: bool) -> Result<Self, Error> {
        let mosq = Mosq::with_id(Handler::new(), id, clean_session)?;
        mosq.start_loop_thread()?;
        Ok(Self {
            mosq: Arc::new(mosq),
        })
    }

    /// Create a new client instance with a random client id
    pub fn with_auto_id() -> Result<Self, Error> {
        let mosq = Mosq::with_auto_id(Handler::new())?;
        mosq.start_loop_thread()?;
        Ok(Self {
            mosq: Arc::new(mosq),
        })
    }

    /// Configure the client with an optional username and password.
    /// The default is `None` for both.
    /// Whether you need to configure these credentials depends on the
    /// broker configuration.
    pub fn set_username_and_password(
        &self,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<(), Error> {
        self.mosq.set_username_and_password(username, password)
    }

    /// Connect to the broker on the specified host and port.
    /// port is typically 1883 for mqtt, but it may be different
    /// in your environment.
    ///
    /// `keep_alive_interval` specifies the interval at which
    /// keepalive requests are sent.  mosquitto has a minimum value
    /// of 5 seconds for this and will generate an error if you use a smaller
    /// value.
    ///
    /// `bind_address` can be used to specify the outgoing interface
    /// for the connection.
    ///
    /// connect completes when the broker acknowledges the CONNECT
    /// command.
    ///
    /// Yields the connection return code; if the status was rejected,
    /// then an Error::RejectedConnection() variant will be returned
    /// so that you don't have to manually check the success.
    pub async fn connect(
        &self,
        host: &str,
        port: c_int,
        keep_alive_interval: Duration,
        bind_address: Option<&str>,
    ) -> Result<ConnectionStatus, Error> {
        let handlers = self.mosq.get_callbacks();
        let (tx, rx) = bounded(1);
        handlers.connect.lock().unwrap().replace(tx);
        self.mosq
            .connect(host, port, keep_alive_interval, bind_address)?;
        let rc = rx
            .recv()
            .await
            .map_err(|_| Error::Mosq(mosq_err_t::MOSQ_ERR_INVAL))?;
        if !rc.is_successful() {
            Err(Error::RejectedConnection(rc))
        } else {
            Ok(rc)
        }
    }

    /// Publish a message to the specified topic.
    ///
    /// The payload size can be 0-283, 435 or 455 bytes; other values
    /// will generate an error result.
    ///
    /// `retain` will set the message to be retained by the broker,
    /// and delivered to new subscribers.
    ///
    /// Returns the assigned MessageId value for the publish.
    pub async fn publish<T: AsRef<str>, P: AsRef<[u8]>>(
        &self,
        topic: T,
        payload: P,
        qos: QoS,
        retain: bool,
    ) -> Result<MessageId, Error> {
        let (tx, rx) = bounded(1);

        {
            let handlers = self.mosq.get_callbacks();
            // Lock the map before we send, so that we can guarantee to
            // win the race with populating the map vs. signalling completion
            let mut mids = handlers.mids.lock().unwrap();
            let mid = self
                .mosq
                .publish(topic.as_ref(), payload.as_ref(), qos, retain)?;
            mids.insert(mid, tx);
        }

        let mid = rx
            .recv()
            .await
            .map_err(|_| Error::Mosq(mosq_err_t::MOSQ_ERR_INVAL))?;

        Ok(mid)
    }

    /// Configure will information for a mosquitto instance.
    /// By default, clients do not have a will.
    /// This must be called before calling `connect`.
    ///
    /// The payload size can be 0-283, 435 or 455 bytes; other values
    /// will generate an error result.
    ///
    /// `retain` will set the message to be retained by the broker,
    /// and delivered to new subscribers.
    pub fn set_last_will<T: AsRef<str>, P: AsRef<[u8]>>(
        &self,
        topic: T,
        payload: P,
        qos: QoS,
        retain: bool,
    ) -> Result<(), Error> {
        self.mosq
            .set_last_will(topic.as_ref(), payload.as_ref(), qos, retain)
    }

    /// Remove a previously configured will.
    /// This must be called before calling connect
    pub fn clear_last_will(&self) -> Result<(), Error> {
        self.mosq.clear_last_will()
    }

    /// Returns a channel that yields messages from topics that this
    /// client has subscribed to.
    /// This method can be called only once; the first time it returns
    /// the channel and subsequently it no longer has the channel
    /// receiver to retur, so will yield None.
    pub fn subscriber(&self) -> Option<Receiver<Event>> {
        let handlers = self.mosq.get_callbacks();
        let x = handlers.subscriber_rx.lock().unwrap().take();
        x
    }

    /// Establish a subscription to topics matching pattern.
    /// The messages will be delivered via the channel returned
    /// via the [subscriber](#method.subscriber) method.
    pub async fn subscribe(&self, pattern: &str, qos: QoS) -> Result<(), Error> {
        let (tx, rx) = bounded(1);

        {
            let handlers = self.mosq.get_callbacks();
            // Lock the map before we send, so that we can guarantee to
            // win the race with populating the map vs. signalling completion
            let mut mids = handlers.mids.lock().unwrap();
            let mid = self.mosq.subscribe(pattern, qos)?;
            mids.insert(mid, tx);
        }

        let _ = rx
            .recv()
            .await
            .map_err(|_| Error::Mosq(mosq_err_t::MOSQ_ERR_INVAL))?;

        Ok(())
    }

    /// Remove subscription(s) for topics that match `pattern`.
    pub async fn unsubscribe(&self, pattern: &str) -> Result<(), Error> {
        let (tx, rx) = bounded(1);

        {
            let handlers = self.mosq.get_callbacks();
            // Lock the map before we send, so that we can guarantee to
            // win the race with populating the map vs. signalling completion
            let mut mids = handlers.mids.lock().unwrap();
            let mid = self.mosq.unsubscribe(pattern)?;
            mids.insert(mid, tx);
        }

        let _ = rx
            .recv()
            .await
            .map_err(|_| Error::Mosq(mosq_err_t::MOSQ_ERR_INVAL))?;

        Ok(())
    }

    /// Set an option for the client.
    /// Most options need to be set prior to calling `connect` in order
    /// to have any effect.
    pub fn set_option(&self, option: &ClientOption) -> Result<(), Error> {
        match option {
            ClientOption::ProtocolVersion(v) => self
                .mosq
                .set_int_option(mosq_opt_t::MOSQ_OPT_PROTOCOL_VERSION, *v as c_int),
            ClientOption::ReceiveMaximum(v) => self
                .mosq
                .set_int_option(mosq_opt_t::MOSQ_OPT_RECEIVE_MAXIMUM, *v as c_int),
            ClientOption::SendMaximum(v) => self
                .mosq
                .set_int_option(mosq_opt_t::MOSQ_OPT_SEND_MAXIMUM, *v as c_int),
            ClientOption::OcspRequired(v) => self.mosq.set_int_option(
                mosq_opt_t::MOSQ_OPT_TLS_OCSP_REQUIRED,
                if *v { 1 } else { 0 },
            ),
            ClientOption::TlsEngine(e) => self
                .mosq
                .set_string_option(mosq_opt_t::MOSQ_OPT_TLS_ENGINE, e),
            ClientOption::TlsKeyForm(e) => self
                .mosq
                .set_string_option(mosq_opt_t::MOSQ_OPT_TLS_KEYFORM, e),
            ClientOption::TlsKPassSha1(e) => self
                .mosq
                .set_string_option(mosq_opt_t::MOSQ_OPT_TLS_ENGINE_KPASS_SHA1, e),
            ClientOption::TlsALPN(e) => self
                .mosq
                .set_string_option(mosq_opt_t::MOSQ_OPT_TLS_ALPN, e),
        }
    }

    /// Configures the TLS parameters for the client.
    ///
    /// `ca_file` is the path to a PEM encoded trust CA certificate file.
    /// Either `ca_file` or `ca_path` must be set.
    ///
    /// `ca_path` is the path to a directory containing PEM encoded trust
    /// CA certificates.  Either `ca_file` or `ca_path` must be set.
    ///
    /// `cert_file` path to a file containing the PEM encoded certificate
    /// file for this client.  If `None` then `key_file` must also be `None`
    /// and no client certificate will be used.
    ///
    /// `key_file` path to a file containing the PEM encoded private key
    /// for this client.  If `None` them `cert_file` must also be `None`
    /// and no client certificate will be used.
    ///
    /// `pw_callback` allows you to provide a password to decrypt an
    /// encrypted key file.  Specify `None` if the key file isn't
    /// password protected.
    pub fn configure_tls<CAFILE, CAPATH, CERTFILE, KEYFILE>(
        &self,
        ca_file: Option<CAFILE>,
        ca_path: Option<CAPATH>,
        cert_file: Option<CERTFILE>,
        key_file: Option<KEYFILE>,
        pw_callback: Option<PasswdCallback>,
    ) -> Result<(), Error>
    where
        CAFILE: AsRef<Path>,
        CAPATH: AsRef<Path>,
        CERTFILE: AsRef<Path>,
        KEYFILE: AsRef<Path>,
    {
        self.mosq
            .configure_tls(ca_file, ca_path, cert_file, key_file, pw_callback)
    }

    /// Disables verification of the server hostname in the server certificate.
    /// If this is disabled, it is impossible to guarantee that the host you
    /// are connecting to is not impersonating your server.  This can be useful
    /// in initial server testing, but makes it possible for a malicious third
    /// party to impersonate your server through DNS spoofing, for example.  Do
    /// not use this function in a real system.  Disabling this makes the
    /// connection encryption pointless.  Must be called before connect.
    pub fn disable_tls_hostname_validation(&self) -> Result<(), Error> {
        self.mosq.set_tls_insecure(true)
    }

    /// Enables verification of the server hostname in the server certificate.
    /// By default this validation is enabled. If this is disabled, it is
    /// impossible to guarantee that the host you are connecting to is not
    /// impersonating your server.  This can be useful in initial server
    /// testing, but makes it possible for a malicious third party to
    /// impersonate your server through DNS spoofing, for example. Must be
    /// called before connect.
    pub fn enable_tls_hostname_validation(&self) -> Result<(), Error> {
        self.mosq.set_tls_insecure(false)
    }

    /// Controls reconnection behavior when running in the message loop.
    /// By default, if a client is unexpectedly disconnected, mosquitto will
    /// try to reconnect.  The default reconnect parameters are to retry once
    /// per second to reconnect.
    ///
    /// You change adjust the delay between connection attempts by changing
    /// the parameters with this function.
    ///
    /// `reconnect_delay` is the base delay amount.
    ///
    /// If `use_exponential_backoff` is true, then the delay is doubled on
    /// each successive attempt, until the `max_reconnect_delay` is reached.
    ///
    /// If `use_exponential_backoff` is false, then the `reconnect_delay` is
    /// added on each successive attempt, until the `max_reconnect_delay` is
    /// reached.
    pub fn set_reconnect_delay(
        &self,
        reconnect_delay: Duration,
        max_reconnect_delay: Duration,
        use_exponential_backoff: bool,
    ) -> Result<(), Error> {
        self.mosq.set_reconnect_delay(
            reconnect_delay,
            max_reconnect_delay,
            use_exponential_backoff,
        )
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn message_debug() {
        let msg_utf8 = Message {
            topic: "topic".to_string(),
            payload: b"hello".to_vec(),
            qos: QoS::AtMostOnce,
            retain: false,
            mid: 1,
        };
        assert_eq!(
            format!("{msg_utf8:?}"),
            "Message { topic: \"topic\", payload: \"hello\", \
            qos: AtMostOnce, retain: false, mid: 1 }"
        );

        let msg_bin = Message {
            topic: "topic".to_string(),
            payload: vec![0x01, 0xa0, 0xc0],
            qos: QoS::AtMostOnce,
            retain: false,
            mid: 1,
        };
        assert_eq!(
            format!("{msg_bin:?}"),
            "Message { topic: \"topic\", payload: [01, A0, C0], \
            qos: AtMostOnce, retain: false, mid: 1 }"
        );
    }
}
