use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::prelude::*;

pub fn setup_tracing(app_name: &str, filter_level: Option<String>) {
    #[cfg(debug_assertions)]
    {
        let tracing_layer = tracing_subscriber::fmt::layer()
            .compact()
            .with_level(true)
            .with_thread_ids(true)
            .with_line_number(true)
            .with_file(true);

        #[cfg(feature = "console")]
        {
            let console_layer = console_subscriber::spawn();
            let filter_layer = if let Some(filter_level) = filter_level {
                EnvFilter::new(filter_level)
            } else {
                EnvFilter::new(format!("{}=debug,subseq_util=debug", app_name))
            };
            tracing_subscriber::registry()
                .with(filter_layer)
                .with(console_layer)
                .with(tracing_layer)
                .init();
            tracing::info!("Tracing started with console");
        }
        #[cfg(not(feature = "console"))]
        {
            let filter_layer = if let Some(filter_level) = filter_level {
                EnvFilter::new(filter_level)
            } else {
                EnvFilter::new(format!("{}=debug,subseq_util=debug", app_name))
            };
            tracing_subscriber::registry()
                .with(filter_layer)
                .with(tracing_layer)
                .init();
        }
    }
    #[cfg(not(debug_assertions))]
    {
        let tracing_layer = tracing_subscriber::fmt::layer().compact().with_level(true);
        let filter_layer = if let Some(filter_level) = filter_level {
            EnvFilter::new(filter_level)
        } else {
            EnvFilter::new(format!("{}=info,subseq_util=info", app_name))
        };
        tracing_subscriber::registry()
            .with(filter_layer)
            .with(tracing_layer)
            .init();
    }
    tracing::info!("App '{}' started", app_name);
}
