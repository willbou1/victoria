// TODO remember to keep an eye on dead code once in a while
#![allow(dead_code)]

mod table;
mod torrents;

use std::{
    env,
};
use tracing_subscriber::{
    EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt
};

use torrents::run_torrents;

#[tokio::main]
async fn main() {
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .without_time()
        .with_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("error"))
                .add_directive("hyper=warn".parse().unwrap())
                .add_directive("reqwest=warn".parse().unwrap())
        );

    #[cfg(debug_assertions)]
    let _guard = {
        use tracing_subscriber::filter::LevelFilter;

        let console_layer = console_subscriber::spawn()
            .with_filter(LevelFilter::TRACE);

        let file_appender = tracing_appender::rolling::never(".", "debug.json");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        let json_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(non_blocking)
            .with_filter(
                EnvFilter::new("debug")
                    .add_directive("hyper=warn".parse().unwrap())
                    .add_directive("reqwest=warn".parse().unwrap())
            );

        tracing_subscriber::registry()
            .with(console_layer)
            .with(json_layer)
            .with(fmt_layer)
            .init();

        guard
    };

    #[cfg(not(debug_assertions))] {
        tracing_subscriber::registry()
            .with(fmt_layer)
            .init();
    }

    let args: Vec<String> = env::args().collect();

    run_torrents(&args[1..]).await.unwrap_or_else(|e| eprintln!("{e}"));
}
