use crate::platform::{self, WindowCommands, WindowVisibility};
use crate::runtime::{Action, ClientHandle, Phase, Protection, Snapshot, SpeedTest};
use eframe::egui::{
    self, Align, Color32, ComboBox, Context, Frame, Layout, Margin, RichText, Stroke, Vec2,
    ViewportCommand,
};
use psimole::tunnel::Service;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const BACKGROUND: Color32 = Color32::from_rgb(19, 27, 24);
const SURFACE: Color32 = Color32::from_rgb(32, 44, 38);
const BORDER: Color32 = Color32::from_rgb(56, 73, 64);
const TEXT: Color32 = Color32::from_rgb(247, 239, 220);
const MUTED: Color32 = Color32::from_rgb(166, 187, 171);
const MINT: Color32 = Color32::from_rgb(121, 225, 157);
const AMBER: Color32 = Color32::from_rgb(251, 204, 108);
const CORAL: Color32 = Color32::from_rgb(255, 162, 143);
const MASCOT: &[u8] = include_bytes!("../../docs/media/psimole.png");

pub fn run(dir: PathBuf, commands: WindowCommands) -> eframe::Result<()> {
    let image = image::load_from_memory(MASCOT)
        .expect("embedded PsiMole PNG")
        .into_rgba8();
    let (width, height) = image.dimensions();
    let icon = egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([440.0, 660.0])
            .with_min_inner_size([366.0, 490.0])
            .with_icon(icon.clone()),
        vsync: true,
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "PsiMole",
        options,
        Box::new(move |creation| Ok(Box::new(Desktop::new(creation, dir, commands, icon)?))),
    )
}

#[derive(Clone, Copy)]
struct Language(bool);

impl Language {
    fn text<'a>(self, english: &'a str, russian: &'a str) -> &'a str {
        if self.0 { russian } else { english }
    }

    fn country(self, code: &str) -> String {
        let names = match code {
            "" => ("Automatic", "Автоматически"),
            "AT" => ("Austria", "Австрия"),
            "AU" => ("Australia", "Австралия"),
            "BE" => ("Belgium", "Бельгия"),
            "CA" => ("Canada", "Канада"),
            "CH" => ("Switzerland", "Швейцария"),
            "CZ" => ("Czechia", "Чехия"),
            "DE" => ("Germany", "Германия"),
            "DK" => ("Denmark", "Дания"),
            "ES" => ("Spain", "Испания"),
            "FI" => ("Finland", "Финляндия"),
            "FR" => ("France", "Франция"),
            "GB" => ("United Kingdom", "Великобритания"),
            "ID" => ("Indonesia", "Индонезия"),
            "IE" => ("Ireland", "Ирландия"),
            "IN" => ("India", "Индия"),
            "IT" => ("Italy", "Италия"),
            "JP" => ("Japan", "Япония"),
            "KR" => ("South Korea", "Южная Корея"),
            "LT" => ("Lithuania", "Литва"),
            "NL" => ("Netherlands", "Нидерланды"),
            "NO" => ("Norway", "Норвегия"),
            "PL" => ("Poland", "Польша"),
            "RO" => ("Romania", "Румыния"),
            "RS" => ("Serbia", "Сербия"),
            "SE" => ("Sweden", "Швеция"),
            "SG" => ("Singapore", "Сингапур"),
            "US" => ("United States", "США"),
            _ => return code.to_owned(),
        };
        self.text(names.0, names.1).to_owned()
    }
}

struct Tray {
    _icon: TrayIcon,
    open: MenuItem,
    quit: MenuItem,
}

impl Tray {
    fn create(
        icon: &egui::IconData,
        hwnd: isize,
        quit: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let image = image::RgbaImage::from_raw(icon.width, icon.height, icon.rgba.clone())
            .expect("embedded icon dimensions");
        let image = image::imageops::resize(&image, 32, 32, image::imageops::FilterType::Lanczos3);
        let menu = Menu::new();
        let open = MenuItem::new("Open PsiMole", true, None);
        let exit = MenuItem::new("Quit", true, None);
        menu.append(&open)?;
        menu.append(&exit)?;
        let tray = TrayIconBuilder::new()
            .with_icon(tray_icon::Icon::from_rgba(image.into_raw(), 32, 32)?)
            .with_tooltip("PsiMole")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;
        let open_id = open.id().clone();
        let exit_id = exit.id().clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id == open_id {
                platform::show_window(hwnd);
            } else if event.id == exit_id {
                quit();
            }
        }));
        let tray_id = tray.id().clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            if matches!(event, TrayIconEvent::Click { id, button: MouseButton::Left, button_state: MouseButtonState::Up, .. } if id == tray_id)
            {
                platform::show_window(hwnd);
            }
        }));
        Ok(Self {
            _icon: tray,
            open,
            quit: exit,
        })
    }

    fn language(&self, language: Language) {
        self.open
            .set_text(language.text("Open PsiMole", "Открыть PsiMole"));
        self.quit.set_text(language.text("Quit", "Выйти"));
    }
}

struct Desktop {
    client: ClientHandle,
    state: Snapshot,
    mascot: egui::TextureHandle,
    tray: Option<Tray>,
    visibility: Option<WindowVisibility>,
    quit: mpsc::Receiver<()>,
    quitting: bool,
}

impl Desktop {
    fn new(
        creation: &eframe::CreationContext<'_>,
        dir: PathBuf,
        commands: WindowCommands,
        icon: egui::IconData,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let context = &creation.egui_ctx;
        style(context);
        let raw = creation.window_handle()?;
        let RawWindowHandle::Win32(win32) = raw.as_raw() else {
            return Err("Expected a Win32 window".into());
        };
        let hwnd = win32.hwnd.get();
        let (client, state) = ClientHandle::start(dir, context.clone())?;
        let (request_quit, quit) = mpsc::channel();
        let close = {
            let context = context.clone();
            move || {
                let _ = request_quit.send(());
                // Showing the hidden viewport lets it process its final update.
                platform::show_window(hwnd);
                context.request_repaint();
            }
        };
        let tray = Tray::create(&icon, hwnd, close.clone()).ok();
        let visibility = Some(WindowVisibility::install(hwnd, tray.is_some())?);
        platform::listen_for_window(commands, hwnd, close);
        if let Some(tray) = &tray {
            tray.language(Language(
                state
                    .settings
                    .russian
                    .unwrap_or_else(platform::default_russian),
            ));
        }
        let mascot = context.load_texture(
            "psimole",
            egui::ColorImage::from(&icon),
            egui::TextureOptions::LINEAR,
        );
        Ok(Self {
            client,
            state,
            mascot,
            tray,
            visibility,
            quit,
            quitting: false,
        })
    }

    fn language(&self) -> Language {
        Language(
            self.state
                .settings
                .russian
                .unwrap_or_else(platform::default_russian),
        )
    }

    fn status(&self, ui: &mut egui::Ui, language: Language) {
        let phase = self.state.phase;
        let (status, accent) = match phase {
            Phase::Off => (language.text("Tunnel closed", "Туннель закрыт"), MUTED),
            Phase::Preparing => (
                language.text("Preparing Psiphon…", "Подготовка Psiphon…"),
                AMBER,
            ),
            Phase::Connecting => (
                language.text("Opening tunnel…", "Открываем туннель…"),
                AMBER,
            ),
            Phase::Connected => (language.text("Tunnel open", "Туннель открыт"), MINT),
            Phase::Reconnecting => (language.text("Reconnecting…", "Переподключаемся…"), AMBER),
            Phase::Failed => (
                language.text("Tunnel unavailable", "Туннель недоступен"),
                CORAL,
            ),
        };
        ui.vertical_centered(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(132.0), egui::Sense::hover());
            let painter = ui.painter();
            painter.circle_filled(rect.center(), 64.0, Color32::from_rgb(25, 41, 33));
            painter.circle_stroke(rect.center(), 64.0, Stroke::new(2.0_f32, accent));
            painter.image(
                self.mascot.id(),
                rect.shrink(11.0),
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            ui.add_space(9.0);
            ui.label(RichText::new(status).size(26.0).strong());
            if let Some(country) = &self.state.tunnel.region {
                let seconds = self
                    .state
                    .tunnel
                    .connected_at
                    .map_or(0, |since| since.elapsed().as_secs());
                ui.label(
                    RichText::new(format!(
                        "{}  ·  {:02}:{:02}:{:02}",
                        language.country(country),
                        seconds / 3600,
                        seconds / 60 % 60,
                        seconds % 60
                    ))
                    .color(accent),
                );
                ui.ctx().request_repaint_after(Duration::from_secs(1));
            } else {
                ui.label(
                    RichText::new(language.country(&self.state.settings.region)).color(accent),
                );
            }
        });
        ui.add_space(10.0);
        let running = !matches!(phase, Phase::Off | Phase::Failed);
        let label = if running {
            language.text("Disconnect", "Отключить")
        } else {
            language.text("Connect", "Подключить")
        };
        let button = egui::Button::new(RichText::new(label).size(17.0).strong().color(BACKGROUND))
            .fill(if running { TEXT } else { MINT })
            .corner_radius(13);
        if ui.add_sized([ui.available_width(), 42.0], button).clicked() {
            self.client.send(if running {
                Action::Disconnect
            } else {
                Action::Connect
            });
        }
        if let Some(error) = &self.state.error {
            ui.label(RichText::new(error).color(CORAL));
        }
        if phase == Phase::Connected {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "↓ {}",
                        byte_rate(self.state.tunnel.received, language)
                    ))
                    .color(MINT),
                );
                ui.label(
                    RichText::new(format!("↑ {}", byte_rate(self.state.tunnel.sent, language)))
                        .color(MUTED),
                );
            });
            ui.horizontal(|ui| {
                let running = matches!(self.state.speed, SpeedTest::Running);
                let label = if running {
                    language.text("Measuring…", "Замеряем…")
                } else {
                    language.text("Measure speed", "Измерить скорость")
                };
                if ui
                    .add_enabled(!running, egui::Button::new(label))
                    .on_hover_text(language.text(
                        "Transfers 32 MB in each direction through this tunnel using four connections.",
                        "Передаёт по 32 МБ в каждую сторону через текущий туннель на четырёх соединениях.",
                    ))
                    .clicked()
                {
                    self.client.send(Action::Measure);
                }
                match self.state.speed {
                    SpeedTest::Measured(result) => {
                        ui.label(
                            RichText::new(format!(
                                "↓ {:.1}   ↑ {:.1} {}",
                                result.down_mbps,
                                result.up_mbps,
                                language.text("Mbps", "Мбит/с")
                            ))
                            .color(MINT),
                        );
                    }
                    SpeedTest::Failed => {
                        ui.label(
                            RichText::new(language.text("Could not measure", "Замер не удался"))
                                .color(CORAL),
                        );
                    }
                    _ => {}
                }
            });
        }
    }

    fn settings(&self, ui: &mut egui::Ui, language: Language) {
        let mut countries: BTreeSet<&str> = self.state.regions.iter().map(String::as_str).collect();
        countries.insert(self.state.settings.region.as_str());
        countries.extend(
            Service::ALL
                .into_iter()
                .filter_map(|service| self.state.settings.countries.get(service)),
        );
        card(ui, |ui| {
            row(
                ui,
                language.text("Exit country", "Страна выхода"),
                |ui| {
                    let mut selected = self.state.settings.region.as_str();
                    ComboBox::from_id_salt("exit")
                        .width(148.0)
                        .selected_text(language.country(selected))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut selected, "", language.country(""));
                            for country in countries
                                .iter()
                                .copied()
                                .filter(|country| !country.is_empty())
                            {
                                ui.selectable_value(
                                    &mut selected,
                                    country,
                                    language.country(country),
                                );
                            }
                        });
                    if selected != self.state.settings.region {
                        self.client.send(Action::Region(selected.to_owned()));
                    }
                },
            );
            ui.separator();
            row(
                ui,
                language.text("System proxy", "Системный прокси"),
                |ui| {
                    let mut enabled = self.state.settings.system_proxy;
                    if ui.checkbox(&mut enabled, "").on_hover_text(language.text(
                    "Route apps that use Windows proxy settings through Psiphon.",
                    "Направляет через Psiphon приложения, которые используют прокси Windows.",
                )).changed() {
                    self.client.send(Action::SystemProxy(enabled));
                }
                },
            );
        });
        ui.add_space(9.0);
        card(ui, |ui| {
            if Service::ALL
                .into_iter()
                .any(|service| self.state.settings.countries.get(service).is_some())
            {
                ui.label(
                    RichText::new(language.text(
                        "Country locks apply through the service proxies below.",
                        "Блокировки работают через прокси сервисов ниже.",
                    ))
                    .small()
                    .color(MUTED),
                );
            }
            for (index, service) in Service::ALL.into_iter().enumerate() {
                if index > 0 {
                    ui.separator();
                }
                self.service(
                    ui,
                    service,
                    self.state.protection[index],
                    &countries,
                    language,
                );
            }
        });
        if let (Some(http), Some(socks)) = (self.state.tunnel.http, self.state.tunnel.socks) {
            ui.label(
                RichText::new(format!("127.0.0.1  ·  HTTP {http}  ·  SOCKS {socks}"))
                    .small()
                    .color(MUTED),
            );
        }
    }

    fn service(
        &self,
        ui: &mut egui::Ui,
        service: Service,
        protection: Protection,
        countries: &BTreeSet<&str>,
        language: Language,
    ) {
        let current = self.state.settings.countries.get(service);
        row(ui, service.label(), |ui| {
            let mut option = current;
            let off = language.text("Lock off", "Без блокировки");
            let text = option.map_or_else(|| off.to_owned(), |country| language.country(country));
            ComboBox::from_id_salt(service.label())
                .width(148.0)
                .selected_text(text)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut option, None, off);
                    for country in countries
                        .iter()
                        .copied()
                        .filter(|country| !country.is_empty())
                    {
                        ui.selectable_value(&mut option, Some(country), language.country(country));
                    }
                });
            if option != current {
                self.client
                    .send(Action::Lock(service, option.map(str::to_owned)));
            }
        });
        if protection == Protection::Disabled {
            return;
        }
        let (label, color) = match protection {
            Protection::Disabled => unreachable!(),
            Protection::Blocked => (language.text("Blocked", "Доступ закрыт"), CORAL),
            Protection::PortUnavailable => (language.text("Port unavailable", "Порт занят"), CORAL),
            Protection::Verifying => (language.text("Verifying exit", "Проверяем страну"), AMBER),
            Protection::Allowed => (language.text("Allowed", "Доступ разрешён"), MINT),
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(label).small().color(color));
            if protection != Protection::PortUnavailable {
                let address = format!("127.0.0.1:{}", service.port());
                let display = RichText::new(&address).monospace().small().color(MUTED);
                if ui.link(display).on_hover_text(language.text(
                    "Copy the HTTP proxy address to use in this service's browser profile or app.",
                    "Скопировать адрес HTTP-прокси для профиля браузера или приложения.",
                )).clicked() {
                    ui.ctx().copy_text(address);
                }
            }
        });
    }
}

impl eframe::App for Desktop {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        if self.quitting {
            return;
        }
        if self.quit.try_recv().is_ok() {
            self.quitting = true;
            self.client.shutdown();
            self.visibility.take();
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        let previous_language = self.language().0;
        self.client.refresh(&mut self.state);
        let language = self.language();
        if previous_language != language.0
            && let Some(tray) = &self.tray
        {
            tray.language(language);
        }
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(Margin::symmetric(19, 12)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("PsiMole").size(17.0).strong());
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.small_button(language.text("RU", "EN")).clicked() {
                                    self.client.send(Action::Language(!language.0));
                                }
                            });
                        });
                        ui.add_space(8.0);
                        self.status(ui, language);
                        ui.add_space(11.0);
                        self.settings(ui, language);
                        if self.tray.is_none() {
                            ui.label(RichText::new(language.text(
                            "The system tray is unavailable. Closing the window stops the tunnel.",
                            "Системный трей недоступен. Закрытие окна остановит туннель.",
                        )).small().color(AMBER));
                        }
                    });
            });
    }
}

fn style(ctx: &Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.override_text_color = Some(TEXT);
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = SURFACE;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(42, 59, 51);
    visuals.widgets.inactive.weak_bg_fill = SURFACE;
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(52, 73, 62);
    visuals.selection.bg_fill = Color32::from_rgb(46, 91, 64);
    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.interact_size.y = 27.0;
    });
}

fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(12)
        .inner_margin(Margin::symmetric(12, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn row(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).strong());
        ui.with_layout(Layout::right_to_left(Align::Center), add);
    });
}

fn byte_rate(bytes: u64, language: Language) -> String {
    if bytes >= 1_048_576 {
        format!(
            "{:.1} {}",
            bytes as f64 / 1_048_576.0,
            language.text("MiB/s", "МиБ/с")
        )
    } else if bytes >= 1024 {
        format!(
            "{:.1} {}",
            bytes as f64 / 1024.0,
            language.text("KiB/s", "КиБ/с")
        )
    } else {
        format!("{bytes} {}", language.text("B/s", "Б/с"))
    }
}
