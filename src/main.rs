mod audio;
mod dsp;
mod engine;
mod font;
mod session;
mod ui;
mod wav;

fn main() {
    std::process::exit(ui::run());
}
