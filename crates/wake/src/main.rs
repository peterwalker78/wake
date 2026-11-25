//! `wake` with no arguments opens the window; with a subcommand it runs that and exits without
//! starting GTK (so `wake , idea` is instant from any shell).
//!
//! The window's own options: `--demo` (made-up data), `--view <monitor|agents|projects|ideas|
//! history|questions|settings>`, `--show <switcher|switcher-subs|session[:id]|project[:id]|pause|stale|stop[:pid]|peek[:session]|questions[:thread]|measure[:width]|behind[:project]|behind-pull[:project]>`,
//! and `--screenshot <png>`, which draws the window into a PNG and quits (run it on GTK's
//! Broadway backend to check the look without opening anything on screen).

mod agents;
mod app;
mod behind;
mod cli;
mod demo;
mod dialogs;
mod disk;
mod fmt;
mod keynav;
mod layout;
mod motion;
mod panel;
mod peek;
mod prefs;
mod questions;
mod scope;
mod setview;
mod sort;
mod source;
mod start;
mod switcher;
mod theme;
mod views;
mod w;
mod why;

const WINDOW_FLAGS: [&str; 4] = ["--demo", "--screenshot", "--view", "--show"];

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let window_args = args.first().is_some_and(|a| WINDOW_FLAGS.contains(&a.as_str()));
    if !window_args && let Some(code) = cli::run(&args) {
        return code;
    }
    app_main(&args)
}

fn usage() -> std::process::ExitCode {
    eprintln!("usage: wake [--demo] [--view monitor|agents|projects|ideas|history|questions|settings] [--show WHAT] [--screenshot PNG]");
    std::process::ExitCode::from(2)
}

// The GTK app's entry point.
fn app_main(args: &[String]) -> std::process::ExitCode {
    let mut opts = app::Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--demo" => opts.demo = true,
            "find" => {
                opts.find = true;
                opts.show = Some("switcher".into());
            }
            "--screenshot" => match it.next() {
                Some(p) => opts.screenshot = Some(p.into()),
                None => return usage(),
            },
            "--view" => match it.next().filter(|v| app::View::from_id(v).is_some()) {
                Some(v) => opts.view = Some(v.clone()),
                None => return usage(),
            },
            "--show" => match it.next() {
                Some(s) => opts.show = Some(s.clone()),
                None => return usage(),
            },
            _ => return usage(),
        }
    }
    let code = app::run(opts);
    std::process::ExitCode::from(u8::from(code))
}
