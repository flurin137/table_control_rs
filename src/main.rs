#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![feature(async_fn_in_trait)]
#![allow(stable_features, unknown_lints, async_fn_in_trait)]

use core::str::from_utf8;
use cyw43_pio::PioSpi;
use defmt::*;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, Stack, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0, PIN_23, PIN_25};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;
use static_cell::make_static;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

const WIFI_NETWORK: &str = "Fx137";
const WIFI_PASSWORD: &str = "asdf1234";

const HTTP_HEADER: &[u8] = b"HTTP/1.0 200 OK\r\nContent-type: text/html\r\n\r\n";

#[embassy_executor::task]
async fn wifi_task(
    runner: cyw43::Runner<
        'static,
        Output<'static, PIN_23>,
        PioSpi<'static, PIN_25, PIO0, 0, DMA_CH0>,
    >,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    stack.run().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let peripherals = embassy_rp::init(Default::default());

    let firmware = include_bytes!("../assets/43439A0.bin");
    let clm = include_bytes!("../assets/43439A0_clm.bin");

    let pwr = Output::new(peripherals.PIN_23, Level::Low);
    let cs = Output::new(peripherals.PIN_25, Level::High);
    let mut pio = Pio::new(peripherals.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        pio.irq0,
        cs,
        peripherals.PIN_24,
        peripherals.PIN_29,
        peripherals.DMA_CH0,
    );

    let state = make_static!(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, firmware).await;
    unwrap!(spawner.spawn(wifi_task(runner)));

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    let config = Config::dhcpv4(Default::default());

    let html = include_bytes!("../assets/index.html");

    let mut table_up = Output::new(peripherals.PIN_20, Level::Low);
    let mut table_down = Output::new(peripherals.PIN_21, Level::Low);

    let seed = 0x0123_4567_89ab_cdef;

    let network_stack = &*make_static!(Stack::new(
        net_device,
        config,
        make_static!(StackResources::<2>::new()),
        seed
    ));

    unwrap!(spawner.spawn(net_task(network_stack)));

    loop {
        match control.join_wpa2(WIFI_NETWORK, WIFI_PASSWORD).await {
            Ok(_) => break,
            Err(err) => {
                info!("join failed with status={}", err.status);
            }
        }
    }

    control.gpio_set(0, true).await;

    info!("waiting for DHCP...");
    while !network_stack.is_config_up() {
        Timer::after_millis(100).await;
    }
    info!("DHCP is now up!");

    control.gpio_set(0, false).await;

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];
    let mut buffer = [0; 4096];

    loop {
        let mut socket = TcpSocket::new(network_stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        if let Err(e) = socket.accept(80).await {
            warn!("accept error: {:?}", e);
            continue;
        }

        info!("Received connection from {:?}", socket.remote_endpoint());

        loop {
            let read_size = match socket.read(&mut buffer).await {
                Ok(0) => {
                    warn!("read EOF");
                    break;
                }
                Ok(read_size) => read_size,
                Err(error) => {
                    warn!("read error: {:?}", error);
                    break;
                }
            };

            let read_data = from_utf8(&buffer[..read_size]).unwrap_or("");

            if read_data.contains("?tableup") {
                table_down.set_low();
                table_up.set_high();
            } else if read_data.contains("?tabledown") {
                table_down.set_high();
                table_up.set_low();
            } else {
                table_down.set_low();
                table_up.set_low();
            }

            info!("rxd {}", read_data);

            if let Err(e) = socket.write_all(HTTP_HEADER).await {
                warn!("write error: {:?}", e);
                break;
            };

            if let Err(e) = socket.write_all(html).await {
                warn!("write error: {:?}", e);
                break;
            };

            socket.close();
        }
    }
}
