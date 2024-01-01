#![cfg_attr(docsrs, feature(doc_cfg))]

//! This crate implements an async MQTT client using libmosquitto.
//!
//! ```no_run
//! use mosquitto_rs::*;
//!
//! fn main() -> Result<(), Error> {
//!     smol::block_on(async {
//!         let mut client = Client::with_auto_id()?;
//!         let rc = client.connect(
//!                        "localhost", 1883,
//!                        std::time::Duration::from_secs(5), None).await?;
//!         println!("connect: {}", rc);
//!
//!         let subscriptions = client.subscriber().unwrap();
//!
//!         client.subscribe("test", QoS::AtMostOnce).await?;
//!         println!("subscribed");
//!
//!         client.publish("test", b"woot", QoS::AtMostOnce, false)
//!             .await?;
//!         println!("published");
//!
//!         if let Ok(msg) = subscriptions.recv().await {
//!             println!("msg: {:?}", msg);
//!         }
//!
//!         Ok(())
//!     })
//! }
//! ```
//!
//! ## Features
//!
//! The following feature flags are available:
//!
//! * `router` - include the router module and `MqttRouter` type. This is on by default.
//! * `vendored-mosquitto` - use bundled libmosquitto 2.4 library. This is on by default.
//! * `vendored-mosquitto-tls` - enable tls support in the bundled libmosquitto. This is on by default.
//! * `vendored-openssl` - build openssl from source, rather than using the system library. Recommended for macOS and Windows users to enable this.
mod client;
mod error;
mod lowlevel;
#[cfg_attr(docsrs, doc(cfg(feature = "router")))]
#[cfg(feature = "router")]
pub mod router;

pub use client::*;
pub use error::*;
pub use lowlevel::*;
