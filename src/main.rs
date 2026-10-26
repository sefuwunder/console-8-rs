mod audio;
mod dsp;
mod engine;
mod font;
mod pw;
mod session;
mod ui;
mod wav;

fn main() {
    std::process::exit(ui::run());
}
