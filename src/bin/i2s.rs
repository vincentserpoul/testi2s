#![no_std]
#![no_main]

use testi2s as _;

// I2S `controller mode` demo
// Generates Morse code audio signals for text from UART, playing back over I2S
// Tested with nRF52840-DK and a UDA1334a DAC

use embedded_hal::digital::v2::{InputPin, OutputPin};
use heapless::consts;
use heapless::spsc::{Consumer, Producer, Queue};
use small_morse::{encode, State};
use {
    hal::{
        gpio::{Input, Level, Output, Pin, PullUp, PushPull},
        gpiote::*,
        i2s::*,
        pac::{TIMER0, UARTE0},
        timer::Timer,
        uarte::*,
    },
    nrf52840_hal as hal,
    rtic::cyccnt::U32Ext,
};

#[repr(align(4))]
struct Aligned<T: ?Sized>(T);

#[rtic::app(device = crate::hal::pac, peripherals = true, monotonic = rtic::cyccnt::CYCCNT)]
const APP: () = {
    struct Resources {
        signal_buf: &'static [i16; 32],
        mute_buf: &'static [i16; 32],
        #[init(None)]
        queue: Option<Queue<State, consts::U256>>,
        producer: Producer<'static, State, consts::U256>,
        consumer: Consumer<'static, State, consts::U256>,
        #[init(5_000_000)]
        speed: u32,
        uarte: Uarte<UARTE0>,
        uarte_timer: Timer<TIMER0>,
        gpiote: Gpiote,
        btn1: Pin<Input<PullUp>>,
        btn2: Pin<Input<PullUp>>,
        led: Pin<Output<PushPull>>,
        transfer: Option<Transfer<&'static [i16; 32]>>,
    }

    #[init(resources = [queue], spawn = [tick])]
    fn init(mut ctx: init::Context) -> init::LateResources {
        // The I2S buffer address must be 4 byte aligned.
        static mut MUTE_BUF: Aligned<[i16; 32]> = Aligned([0i16; 32]);
        static mut SIGNAL_BUF: Aligned<[i16; 32]> = Aligned([0i16; 32]);

        // Fill signal buffer with triangle waveform, 2 channels interleaved
        let len = SIGNAL_BUF.0.len() / 2;

        for x in 0..len {
            SIGNAL_BUF.0[2 * x] = triangle_wave(x as i32, len, 2048, 0, 1) as i16;
            SIGNAL_BUF.0[2 * x + 1] = triangle_wave(x as i32, len, 2048, 0, 1) as i16;
        }

        let _clocks = hal::clocks::Clocks::new(ctx.device.CLOCK).enable_ext_hfosc();
        // Enable the monotonic timer (CYCCNT)
        ctx.core.DCB.enable_trace();
        ctx.core.DWT.enable_cycle_counter();

        let p0 = hal::gpio::p0::Parts::new(ctx.device.P0);

        let sck_pin = p0.p0_02.into_push_pull_output(Level::Low).degrade();
        let sdout_pin = p0.p0_01.into_push_pull_output(Level::Low).degrade();
        let lrck_pin = p0.p0_03.into_push_pull_output(Level::Low).degrade();

        let i2s = I2S::new_controller(
            ctx.device.I2S,
            None,
            &sck_pin,
            &lrck_pin,
            None,
            Some(&sdout_pin),
        );
        i2s.start();

        // Configure buttons
        let btn1 = p0.p0_11.into_pullup_input().degrade();
        let btn2 = p0.p0_12.into_pullup_input().degrade();
        let gpiote = Gpiote::new(ctx.device.GPIOTE);
        gpiote.port().input_pin(&btn1).low();
        gpiote.port().input_pin(&btn2).low();
        gpiote.port().enable_interrupt();

        // Configure the onboard USB CDC UARTE
        let uarte = Uarte::new(
            ctx.device.UARTE0,
            Pins {
                txd: p0.p0_06.into_push_pull_output(Level::High).degrade(),
                rxd: p0.p0_08.into_floating_input().degrade(),
                cts: None,
                rts: None,
            },
            Parity::EXCLUDED,
            Baudrate::BAUD115200,
        );

        *ctx.resources.queue = Some(Queue::new());
        let (producer, consumer) = ctx.resources.queue.as_mut().unwrap().split();

        defmt::info!("Morse code generator");
        defmt::info!("Send me text over UART @ 115_200 baud");
        defmt::info!("Press button 1 to slow down or button 2 to speed up");

        ctx.spawn.tick().ok();

        init::LateResources {
            producer,
            consumer,
            gpiote,
            btn1,
            btn2,
            led: p0.p0_13.into_push_pull_output(Level::High).degrade(),
            uarte,
            uarte_timer: Timer::new(ctx.device.TIMER0),
            transfer: i2s.tx(&MUTE_BUF.0).ok(),
            signal_buf: &SIGNAL_BUF.0,
            mute_buf: &MUTE_BUF.0,
        }
    }

    #[idle(resources=[uarte, uarte_timer, producer])]
    fn idle(ctx: idle::Context) -> ! {
        let idle::Resources {
            uarte,
            uarte_timer,
            producer,
        } = ctx.resources;
        let uarte_rx_buf = &mut [0u8; 64][..];
        loop {
            match uarte.read_timeout(uarte_rx_buf, uarte_timer, 100_000) {
                Ok(_) => {
                    if let Ok(msg) = core::str::from_utf8(&uarte_rx_buf[..]) {
                        defmt::info!("{}", msg);
                        for action in encode(msg) {
                            for _ in 0..action.duration {
                                producer.enqueue(action.state).ok();
                            }
                        }
                    }
                }
                Err(hal::uarte::Error::Timeout(n)) if n > 0 => {
                    if let Ok(msg) = core::str::from_utf8(&uarte_rx_buf[..n]) {
                        defmt::info!("{}", msg);
                        for action in encode(msg) {
                            for _ in 0..action.duration {
                                producer.enqueue(action.state).ok();
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    #[task(resources = [consumer, transfer, signal_buf, mute_buf, led, speed], schedule = [tick])]
    fn tick(ctx: tick::Context) {
        let (_buf, i2s) = ctx.resources.transfer.take().unwrap().wait();
        match ctx.resources.consumer.dequeue() {
            Some(State::On) => {
                // Move TX pointer to signal buffer (sound ON)
                *ctx.resources.transfer = i2s.tx(*ctx.resources.signal_buf).ok();
                ctx.resources.led.set_low().ok();
            }
            _ => {
                // Move TX pointer to silent buffer (sound OFF)
                *ctx.resources.transfer = i2s.tx(*ctx.resources.mute_buf).ok();
                ctx.resources.led.set_high().ok();
            }
        }
        ctx.schedule
            .tick(ctx.scheduled + ctx.resources.speed.cycles())
            .ok();
    }

    #[task(binds = GPIOTE, resources = [gpiote, speed], schedule = [debounce])]
    fn on_gpiote(ctx: on_gpiote::Context) {
        ctx.resources.gpiote.reset_events();
        ctx.schedule.debounce(ctx.start + 3_000_000.cycles()).ok();
    }

    #[task(resources = [btn1, btn2, speed])]
    fn debounce(ctx: debounce::Context) {
        if ctx.resources.btn1.is_low().unwrap() {
            defmt::info!("Go slower");
            *ctx.resources.speed += 600_000;
        }
        if ctx.resources.btn2.is_low().unwrap() {
            defmt::info!("Go faster");
            *ctx.resources.speed -= 600_000;
        }
    }

    extern "C" {
        fn SWI0_EGU0();
        fn SWI1_EGU1();
    }
};

fn triangle_wave(x: i32, length: usize, amplitude: i32, phase: i32, periods: i32) -> i32 {
    let length = length as i32;
    amplitude
        - ((2 * periods * (x + phase + length / (4 * periods)) * amplitude / length)
            % (2 * amplitude)
            - amplitude)
            .abs()
        - amplitude / 2
}
