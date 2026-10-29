// TODO remember to keep an eye on dead code once in a while
#![allow(dead_code)]

mod torrents;
mod rows;
mod libvictoria;
mod util;

use anyhow::Result;
use std::{
    env,
};
use tracing_subscriber::{
    EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt, filter::LevelFilter,
};

use torrents::run_torrents;

#[tokio::main]
async fn main() -> Result<()> {
    let file_appender = tracing_appender::rolling::never(".", "debug.json");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    #[cfg(debug_assertions)]
    let _guard = {
        let console_layer = console_subscriber::spawn()
            .with_filter(LevelFilter::TRACE);

        let json_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(non_blocking)
            .with_filter(
                EnvFilter::new("trace")
                    .add_directive("hyper=warn".parse()?)
                    .add_directive("reqwest=warn".parse()?)
            );

        tracing_subscriber::registry()
            .with(console_layer)
            .with(json_layer)
            .init();

        guard
    };

    #[cfg(not(debug_assertions))] {
        let json_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(non_blocking)
            .with_filter(
                EnvFilter::new("info")
                    .add_directive("hyper=warn".parse()?)
                    .add_directive("reqwest=warn".parse()?)
            );

        tracing_subscriber::registry()
            .with(json_layer)
            .init();
    }

    let args: Vec<String> = env::args().collect();

    run_torrents(&args[1..]).await?;

    Ok(())
}
