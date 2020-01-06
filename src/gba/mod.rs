use std::boxed::Box;
use std::default::Default;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::mem;
use std::path::Path;
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

use flame;

use sdl2;
use sdl2::audio::{AudioDevice, AudioSpecDesired};
use sdl2::keyboard::Scancode;
use sdl2::pixels::PixelFormatEnum;
use sdl2::render::{Canvas, Texture, TextureCreator};
use sdl2::video::{Window, WindowContext};
use sdl2::Sdl;

use shared::Shared;

use Result;

use cpu::Cpu;
use io::key::KeyState;
use io::ppu::{Ppu, COLS, ROWS};
use io::spu::{SoundBuf, Spu, FREQ, SAMPLES};
use io::IoReg;
use mmu::gba::Gba as GbaMmu;
use rom::GameRom;

mod save_state;

const CYCLES_PER_SEC: u64 = 16 * 1024 * 1024;
const CYCLES_PER_FRAME: u64 = 280896;

#[derive(Clone, Debug)]
pub struct Options {
    pub fps_limit: bool,
    pub breaks: Vec<u32>,
    pub step_frames: bool,
    pub direct_boot: bool,
    pub save_file: OsString,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            fps_limit: true,
            breaks: Default::default(),
            step_frames: false,
            direct_boot: false,
            save_file: OsStr::new("gba").to_os_string(),
        }
    }
}

/// Parent container for all components of the system
pub struct Gba<'a> {
    opts: Options,

    pub ctx: Sdl,

    canvas: Canvas<Window>,
    texture_creator: TextureCreator<WindowContext>,
    texture: Texture<'a>,
    audio: AudioDevice<SoundBuf>,

    cpu: Cpu<GbaMmu<'a>>,
    mmu: GbaMmu<'a>,
    io: IoReg<'a>,
    ppu: Ppu<'a>,
    spu: Spu<'a>,
}

impl<'a> Gba<'a> {
    pub fn new(rom: GameRom, bios: GameRom, options: Options) -> Box<Self> {
        unsafe {
            let mut gba: Box<Gba> = Box::new(mem::uninitialized());
            ptr::write(&mut gba.opts, options);

            ptr::write(&mut gba.ctx, sdl2::init().unwrap());
            let video = gba.ctx.video().unwrap();
            let window = video
                .window("GBA", 720, 480)
                .position_centered()
                .build()
                .unwrap();

            ptr::write(&mut gba.canvas, window.into_canvas().build().unwrap());
            gba.canvas.set_logical_size(COLS, ROWS).unwrap();
            ptr::write(&mut gba.texture_creator, gba.canvas.texture_creator());
            info!(
                "Default pixel format: {:?}",
                gba.texture_creator.default_pixel_format()
            );
            ptr::write(
                &mut gba.texture,
                mem::transmute(
                    gba.texture_creator
                        .create_texture_streaming(PixelFormatEnum::RGB888, COLS, ROWS)
                        .unwrap(),
                ),
            );

            ptr::write(&mut gba.io, IoReg::new());
            ptr::write(
                &mut gba.mmu,
                GbaMmu::new(rom, bios, Shared::new(&mut gba.io)),
            );

            ptr::write(&mut gba.cpu, Cpu::new(Shared::new(&mut gba.mmu), &[]));
            if gba.opts.direct_boot {
                gba.cpu.init_direct();
            } else {
                gba.cpu.init_arm();
            }
            let opts = Shared::new(&mut gba.opts);
            gba.cpu.set_breaks(opts.breaks.iter());

            ptr::write(
                &mut gba.ppu,
                Ppu::new(
                    Shared::new(&mut gba.texture),
                    Shared::new(&mut gba.io),
                    Shared::new(&mut gba.mmu),
                ),
            );

            ptr::write(&mut gba.spu, Spu::new(Shared::new(&mut gba.io)));

            let desired_spec = AudioSpecDesired {
                freq: Some(FREQ),
                channels: Some(2),
                samples: Some((SAMPLES * 2) as u16),
            };
            let audio = gba.ctx.audio().unwrap();
            let device = audio
                .open_playback(None, &desired_spec, |spec| {
                    warn!("Audio spec: {:?}", spec);
                    gba.spu.get_callback()
                })
                .unwrap();
            ptr::write(&mut gba.audio, device);
            gba.audio.resume();

            let cpu = Shared::new(&mut gba.cpu);
            let ppu = Shared::new(&mut gba.ppu);
            gba.mmu.init(cpu);
            gba.io.init(cpu, Shared::new(&mut gba.mmu), ppu);

            gba
        }
    }

    pub fn run(&mut self) -> Result<()> {
        let mut frame = 0;
        let mut event_pump = self.ctx.event_pump().unwrap();

        let frame_duration = Duration::new(
            0,
            ((1_000_000_000u64 * CYCLES_PER_FRAME) / CYCLES_PER_SEC) as u32,
        );
        let mut prev_time = Instant::now();
        loop {
            let _guard = flame::start_guard("frame cycle");
            let start = Instant::now();

            flame::span_of("frame emu", || self.emulate_frame());
            flame::span_of("frame copy", || {
                self.canvas.copy(&self.texture, None, None).unwrap()
            });
            flame::span_of("frame present", || self.canvas.present());

            {
                event_pump.pump_events();
                let keys = event_pump.keyboard_state();
                self.io.set_keyreg(&KeyState::new_from_keystate(&keys));

                if keys.is_scancode_pressed(Scancode::Escape) {
                    break;
                }
                if keys.is_scancode_pressed(Scancode::B) {
                    log::set_max_level(match log::max_level() {
                        log::LevelFilter::Debug => log::LevelFilter::Error,
                        _ => log::LevelFilter::Debug,
                    });
                }
            }
            loop {
                let ctrl = {
                    let keys = event_pump.keyboard_state();
                    keys.is_scancode_pressed(Scancode::LCtrl)
                        || keys.is_scancode_pressed(Scancode::RCtrl)
                };
                if let Some(event) = event_pump.poll_event() {
                    if let sdl2::event::Event::KeyDown { scancode, .. } = event {
                        if let Some(code) = scancode {
                            self.check_save(code, ctrl);
                        }
                    }
                } else {
                    break;
                }
            }
            if self.opts.step_frames {
                info!("Frame: {}", frame);
                loop {
                    let event = event_pump.wait_event();
                    if let sdl2::event::Event::KeyDown { scancode, .. } = event {
                        if scancode == Some(Scancode::F) {
                            break;
                        }
                    }
                }
            }

            let end = Instant::now();
            if self.opts.fps_limit {
                if end < prev_time + frame_duration {
                    let sleep_time = (prev_time + frame_duration) - end;
                    thread::sleep(sleep_time);
                }
            }
            prev_time = prev_time + frame_duration;

            let now = Instant::now();
            info!("{} fps", 1_000_000_000u32 / ((now - start).subsec_nanos()));
            frame += 1;
        }
        Ok(())
    }

    fn emulate_frame(&mut self) {
        for _ in 0..CYCLES_PER_FRAME {
            self.cycle();
        }
    }

    fn cycle(&mut self) {
        self.cpu.cycle();
        self.ppu.cycle();
        self.spu.cycle();
        self.io.cycle();
    }
}
