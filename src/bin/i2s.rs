#![no_std]
#![no_main]

use testi2s as _;

// I2S `controller mode` demo
// Generates Morse code audio signals for text from UART, playing back over I2S
// Tested with nRF52840-DK and a UDA1334a DAC

use embedded_hal::blocking::delay::DelayMs;
use embedded_hal::digital::v2::{InputPin, OutputPin};
use heapless::consts;
use heapless::spsc::{Consumer, Producer, Queue};
use nrf52840_hal::{
    self as hal,
    gpio::{Input, Level, Output, Pin, PullUp, PushPull},
    gpiote::*,
    i2s::*,
    pac::TIMER0,
    timer::Timer,
};

use small_morse::{encode, State};

use rtic::cyccnt::U32Ext;

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
        gpiote: Gpiote,
        timr: Timer<TIMER0>,
        btn1: Pin<Input<PullUp>>,
        btn2: Pin<Input<PullUp>>,
        btn3: Pin<Input<PullUp>>,
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

        let timr: Timer<TIMER0> = Timer::new(ctx.device.TIMER0);

        let p0 = hal::gpio::p0::Parts::new(ctx.device.P0);

        // std out
        let stdout = p0.p0_01.into_push_pull_output(Level::Low).degrade();
        // sck
        let bclk = p0.p0_02.into_push_pull_output(Level::Low).degrade();
        // lrck
        let lrc = p0.p0_03.into_push_pull_output(Level::Low).degrade();

        let i2s = I2S::new_controller(ctx.device.I2S, None, &bclk, &lrc, None, Some(&stdout));
        i2s.set_ratio(Ratio::_256x);
        i2s.set_mck_frequency(MckFreq::_32MDiv8);
        i2s.start();

        // Configure buttons
        let btn1 = p0.p0_11.into_pullup_input().degrade();
        let btn2 = p0.p0_12.into_pullup_input().degrade();
        let btn3 = p0.p0_10.into_pullup_input().degrade();

        let gpiote = Gpiote::new(ctx.device.GPIOTE);
        gpiote.port().input_pin(&btn1).low();
        gpiote.port().input_pin(&btn2).low();
        gpiote.port().input_pin(&btn3).low();
        gpiote.port().enable_interrupt();

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
            timr,
            btn1,
            btn2,
            btn3,
            led: p0.p0_13.into_push_pull_output(Level::High).degrade(),
            transfer: i2s.tx(&MUTE_BUF.0).ok(),
            signal_buf: &SIGNAL_BUF.0,
            mute_buf: &MUTE_BUF.0,
        }
    }

    #[idle(resources=[producer,timr])]
    fn idle(ctx: idle::Context) -> ! {
        let idle::Resources { producer, timr } = ctx.resources;
        let msg = "ilovepeanuts";
        loop {
            defmt::info!("enqueuing msg");
            for action in encode(msg) {
                for _ in 0..action.duration {
                    producer.enqueue(action.state).ok();
                }
            }
            timr.delay_ms(1000u32);
        }
    }

    #[task(resources = [consumer, transfer, signal_buf, mute_buf, led, speed], schedule = [tick])]
    fn tick(ctx: tick::Context) {
        let (_buf, i2s) = ctx.resources.transfer.take().unwrap().wait();
        match ctx.resources.consumer.dequeue() {
            Some(State::On) => {
                defmt::info!("transmitting msg");
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
