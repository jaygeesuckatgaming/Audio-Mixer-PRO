use std::sync::Arc;

use virtual_cables::usbip::spawn_server;
use virtual_cables::{self_test, Config, Device, LogFn, Mode};

fn log_fn() -> LogFn {
    Arc::new(|line| println!("{line}"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if let Some(position) = args
        .iter()
        .position(|arg| arg == "--attach-broker" || arg == "--vc-attach-broker")
    {
        let address = args.get(position + 1).cloned().unwrap_or_default();
        let token = args.get(position + 2).cloned().unwrap_or_default();
        let log = log_fn();
        #[cfg(windows)]
        {
            if let Err(err) = virtual_cables::win::run_attach_broker(&address, &token, &log) {
                eprintln!("attach broker failed: {err}");
                std::process::exit(1);
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (address, token, log);
            eprintln!("the attach broker is Windows-only");
            std::process::exit(1);
        }
        return;
    }

    let mut config = Config::import_legacy().unwrap_or_default();
    let mut listen = "127.0.0.1:3240".to_string();
    let mut serve = false;
    let mut attach = false;
    let mut test = false;
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "--cables" => {
                index += 1;
                if let Some(value) = args.get(index).and_then(|v| v.parse::<u8>().ok()) {
                    config.cables = value;
                }
            }
            "--buffer-ms" => {
                index += 1;
                if let Some(value) = args.get(index).and_then(|v| v.parse::<u32>().ok()) {
                    config.buffer_ms = value;
                }
            }
            "--max-rate" => {
                index += 1;
                if let Some(value) = args.get(index).and_then(|v| v.parse::<u32>().ok()) {
                    config.max_rate = value;
                }
            }
            "--uac1" => config.uac_mode = Mode::Uac1,
            "--uac2" => config.uac_mode = Mode::Uac2,
            "--listen" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    listen = value.clone();
                }
            }
            "--self-test" => test = true,
            "--serve" => serve = true,
            "--attach" => attach = true,
            "--help" | "-h" => {
                usage();
                return;
            }
            other => {
                eprintln!("unknown argument: {other}");
                usage();
                std::process::exit(2);
            }
        }
        index += 1;
    }
    config = config.normalized();

    if test {
        match self_test(&config) {
            Ok(report) => {
                print!("{report}");
                println!("self-test passed");
            }
            Err(err) => {
                eprintln!("self-test failed: {err}");
                std::process::exit(1);
            }
        }
        return;
    }

    if !serve {
        usage();
        return;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    runtime.block_on(async move {
        let log = log_fn();
        let mut devices = Vec::with_capacity(config.cables as usize);
        for number in 1..=config.cables {
            match Device::new(number, config.buffer_ms, config.uac_mode, config.max_rate) {
                Ok(device) => devices.push(Arc::new(device)),
                Err(err) => {
                    eprintln!("could not create cable {number}: {err}");
                    return;
                }
            }
        }
        let handle = spawn_server(listen.clone(), devices, log.clone());
        let _ = handle.ready.await;
        if attach {
            let log = log.clone();
            let cables = config.cables;
            std::thread::spawn(move || {
                #[cfg(windows)]
                match virtual_cables::win::start_broker(&log) {
                    Ok(mut broker) => match broker.sync(cables) {
                        Ok(()) => log(format!("attached {cables} cable(s)")),
                        Err(err) => log(format!("attach failed: {err}")),
                    },
                    Err(err) => log(format!("administrator broker failed: {err}")),
                }
                #[cfg(not(windows))]
                {
                    let _ = (log, cables);
                }
            });
        }
        log("press Ctrl+C to stop".to_string());
        let _ = tokio::signal::ctrl_c().await;
        handle.shutdown.cancel();
        let _ = handle.join.await;
    });
}

fn usage() {
    println!(
        "vc --self-test [--cables N] [--max-rate HZ] [--uac1]\n\
         vc --serve [--attach] [--listen ADDR] [--cables N] [--max-rate HZ] [--uac1]\n\
         vc --vc-attach-broker ADDR TOKEN"
    );
}
