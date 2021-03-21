#![no_main]
#![no_std]

use testi2s as _; // global logger + panicking-behavior + memory layout

#[cortex_m_rt::entry]
fn main() -> ! {
    defmt::info!("Hello, world!");

    testi2s::exit()
}
