use super::theme::{
    AMBER, BLUE, BLUE_TEXT, CORAL, DIVIDER, GREEN, MUTED, SURFACE, TEXT, regular, semibold,
};
use super::{Desktop, Language};
use crate::platform;
use crate::runtime::{Action, Phase, Protection, SpeedTest};
use eframe::egui::{
    self, Align, Button, Color32, ComboBox, FontId, Frame, Layout, Margin, Rect, Response, Sense,
    Stroke, StrokeKind, Ui, WidgetInfo, WidgetType, pos2, vec2,
};
use psimole::tunnel::{REGIONS, Service};
use std::collections::BTreeSet;
use std::time::Duration;

impl Desktop {
    pub(super) fn panel(&self, ui: &mut Ui, language: Language) {
        self.hero(ui, language);
        ui.add_space(12.0);
        self.metrics(ui, language);
        ui.add_space(11.0);
        self.settings(ui, language);

        if let (Some(http), Some(socks)) = (self.state.tunnel.http, self.state.tunnel.socks) {
            ui.add_space(9.0);
            ui.vertical_centered(|ui| {
                ui.label(regular(
                    format!("HTTP {http}  ·  SOCKS {socks}"),
                    10.5,
                    MUTED,
                ))
                .on_hover_text(format!("127.0.0.1:{http}  ·  127.0.0.1:{socks}"));
            });
        }
        if self.tray.is_none() {
            ui.add_space(8.0);
            ui.label(regular(
                language.text(
                    "Closing the window stops the tunnel when the system tray is unavailable.",
                    "Трей недоступен. Закрытие окна остановит туннель.",
                ),
                11.0,
                AMBER,
            ));
        }
    }

    fn hero(&self, ui: &mut Ui, language: Language) {
        let phase = self.state.phase;
        let (title, accent) = match phase {
            Phase::Off => (language.text("Tunnel closed", "Туннель закрыт"), MUTED),
            Phase::Preparing => (
                language.text("Preparing Psiphon…", "Подготовка Psiphon…"),
                AMBER,
            ),
            Phase::Connecting => (
                language.text("Opening tunnel…", "Открываем туннель…"),
                AMBER,
            ),
            Phase::Connected => (language.text("Tunnel open", "Туннель открыт"), GREEN),
            Phase::Reconnecting => (language.text("Reconnecting…", "Переподключаемся…"), AMBER),
            Phase::Failed => (
                language.text("Tunnel unavailable", "Туннель недоступен"),
                CORAL,
            ),
        };
        let actual_country = self.state.tunnel.region.as_deref();
        let country = actual_country.unwrap_or(&self.state.settings.region);
        let mut caption = language.country(country);
        if let Some(since) = self.state.tunnel.connected_at {
            let elapsed = since.elapsed();
            caption.push_str(" · ");
            caption.push_str(&uptime(elapsed, language));
            let seconds = elapsed.as_secs();
            let next = if seconds < 60 { 1 } else { 60 - seconds % 60 };
            ui.ctx().request_repaint_after(
                Duration::from_secs(next) - Duration::from_nanos(elapsed.subsec_nanos().into()),
            );
        }

        let (art_row, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), 154.0), Sense::hover());
        self.art.hero(
            ui,
            Rect::from_center_size(art_row.center(), vec2(154.0, 154.0)),
            phase,
            actual_country,
            matches!(
                phase,
                Phase::Preparing | Phase::Connecting | Phase::Reconnecting
            ) && platform::animate_window(self.hwnd),
        );
        let language_rect =
            Rect::from_min_size(art_row.right_top() - vec2(36.0, -1.0), vec2(36.0, 24.0));
        let language_button =
            Button::new(semibold(language.text("RU", "EN"), 11.0, MUTED)).corner_radius(7);
        if ui
            .place(language_rect, language_button)
            .on_hover_text(language.text("Русский", "English"))
            .clicked()
        {
            self.client.send(Action::Language(!language.0));
        }

        ui.vertical_centered(|ui| {
            ui.add_space(2.0);
            ui.label(semibold(title, 22.0, TEXT));
            ui.add_space(3.0);
            let galley = ui.fonts_mut(|fonts| {
                fonts.layout_no_wrap(caption, FontId::proportional(12.0), accent)
            });
            let has_flag = REGIONS.contains(&country);
            let width = galley.size().x + if has_flag { 22.0 } else { 0.0 };
            let (line, _) = ui.allocate_exact_size(vec2(width, galley.size().y), Sense::hover());
            if has_flag {
                self.art
                    .flag(ui, line.left_center() + vec2(9.0, 0.0), country);
            }
            ui.painter().galley(
                line.right_top() - vec2(galley.size().x, 0.0),
                galley,
                accent,
            );
        });

        ui.add_space(13.0);
        let running = !matches!(phase, Phase::Off | Phase::Failed);
        let response = ui
            .scope(|ui| {
                let idle = if running {
                    Color32::from_rgb(48, 49, 57)
                } else {
                    BLUE
                };
                let hover = if running {
                    Color32::from_rgb(57, 58, 67)
                } else {
                    Color32::from_rgb(33, 114, 222)
                };
                let widgets = &mut ui.visuals_mut().widgets;
                widgets.inactive.weak_bg_fill = idle;
                widgets.hovered.weak_bg_fill = hover;
                widgets.active.weak_bg_fill = hover;
                ui.add(
                    Button::new(semibold(
                        if running {
                            language.text("Disconnect", "Выключить")
                        } else {
                            language.text("Connect", "Включить")
                        },
                        14.0,
                        if running { CORAL } else { Color32::WHITE },
                    ))
                    .min_size(vec2(ui.available_width(), 39.0))
                    .corner_radius(20),
                )
            })
            .inner;
        if response.clicked() {
            self.client.send(if running {
                Action::Disconnect
            } else {
                Action::Connect
            });
        }
        if let Some(error) = &self.state.error {
            ui.add_space(6.0);
            ui.label(regular(error, 11.0, CORAL));
        }
    }

    fn metrics(&self, ui: &mut Ui, language: Language) {
        let width = (ui.available_width() - ui.spacing().item_spacing.x) * 0.5;
        let tunnel = &self.state.tunnel;
        let connected = self.state.phase == Phase::Connected;
        ui.horizontal(|ui| {
            let country = tunnel
                .region
                .as_deref()
                .unwrap_or(&self.state.settings.region);
            tile(ui, width, language.text("Server", "Сервер"), |ui| {
                let country = language.country(country);
                ui.add(egui::Label::new(semibold(&country, 16.0, TEXT)).truncate())
                    .on_hover_text(country);
                ui.label(regular(
                    if self.state.settings.region.is_empty() {
                        language.text("Auto selection", "Автовыбор")
                    } else {
                        language.text("Your choice", "Твой выбор")
                    },
                    11.0,
                    MUTED,
                ));
            });
            tile(ui, width, language.text("Uptime", "Открыт"), |ui| {
                ui.label(semibold(
                    tunnel
                        .connected_at
                        .map_or("—".to_owned(), |since| uptime(since.elapsed(), language)),
                    16.0,
                    TEXT,
                ));
                ui.label(regular(
                    language.text("This connection", "Текущее соединение"),
                    11.0,
                    MUTED,
                ));
            });
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            tile(ui, width, language.text("Traffic", "Трафик"), |ui| {
                if connected {
                    ui.label(semibold(
                        format!("↓ {}", byte_rate(tunnel.received, language)),
                        16.0,
                        TEXT,
                    ));
                    ui.label(regular(
                        format!("↑ {}", byte_rate(tunnel.sent, language)),
                        11.0,
                        MUTED,
                    ));
                } else {
                    ui.label(semibold("—", 16.0, TEXT));
                }
            });
            tile(
                ui,
                width,
                language.text("VPN speed", "Скорость VPN"),
                |ui| {
                    self.speed(ui, language, connected);
                },
            );
        });
    }

    fn speed(&self, ui: &mut Ui, language: Language, connected: bool) {
        let measure =
            |ui: &mut Ui, label, size| {
                if ui.link(semibold(label, size, BLUE_TEXT)).on_hover_text(language.text(
                "Measures through this tunnel using four connections. Up to 36 MB each way.",
                "Замер через этот туннель на четырёх соединениях. До 36 МБ в каждую сторону.",
            )).clicked() {
                self.client.send(Action::Measure);
            }
            };
        if !connected {
            ui.label(semibold("—", 16.0, TEXT));
            ui.label(regular(
                language.text("Connect to measure", "После подключения"),
                11.0,
                MUTED,
            ));
            return;
        }
        match self.state.speed {
            SpeedTest::Idle => measure(ui, language.text("Measure", "Измерить"), 15.0),
            SpeedTest::Running => {
                ui.label(semibold(
                    language.text("Measuring…", "Замеряем…"),
                    14.0,
                    AMBER,
                ));
            }
            SpeedTest::Measured(result) => {
                let unit = language.text("Mbps", "Мбит/с");
                ui.label(semibold(
                    format!("↓ {:.1} {unit}", result.down_mbps),
                    14.0,
                    GREEN,
                ));
                ui.horizontal(|ui| {
                    ui.label(regular(
                        format!("↑ {:.1} {unit}", result.up_mbps),
                        11.0,
                        MUTED,
                    ));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        measure(ui, language.text("Again", "Ещё"), 11.0);
                    });
                });
            }
            SpeedTest::Failed => {
                ui.label(regular(
                    language.text("Could not measure", "Замер не удался"),
                    12.0,
                    CORAL,
                ));
                measure(ui, language.text("Retry", "Повторить"), 11.0);
            }
        }
    }

    fn settings(&self, ui: &mut Ui, language: Language) {
        let mut countries: BTreeSet<&str> = self.state.regions.iter().map(String::as_str).collect();
        countries.insert(self.state.settings.region.as_str());
        countries.extend(
            Service::ALL
                .into_iter()
                .filter_map(|service| self.state.settings.countries.get(service)),
        );
        countries.remove("");
        let current =
            (!self.state.settings.region.is_empty()).then_some(self.state.settings.region.as_str());

        Frame::new().fill(SURFACE).corner_radius(15).show(ui, |ui| {
            setting_row(
                ui,
                language.text("Country", "Страна"),
                |ui| {
                    let (response, option) = self.country_choice(
                        ui,
                        "exit",
                        current,
                        language.text("Automatic", "Автоматически"),
                        &countries,
                        language,
                    );
                    if option != current {
                        self.client
                            .send(Action::Region(option.unwrap_or_default().to_owned()));
                    }
                    response
                },
                |_| {},
            );

            for (index, service) in Service::ALL.into_iter().enumerate() {
                divider(ui);
                setting_row(
                    ui,
                    service.label(),
                    |ui| {
                        let current = self.state.settings.countries.get(service);
                        let (response, option) = self.country_choice(
                            ui,
                            service.label(),
                            current,
                            language.text("Lock off", "Без привязки"),
                            &countries,
                            language,
                        );
                        if option != current {
                            self.client
                                .send(Action::Lock(service, option.map(str::to_owned)));
                        }
                        response
                    },
                    |ui| self.protection(ui, service, self.state.protection[index], language),
                );
            }

            divider(ui);
            setting_row(
                ui,
                language.text("System proxy", "Системный прокси"),
                |ui| self.system_proxy(ui, language),
                |ui| {
                    ui.label(regular(
                        language.text(
                            "For apps using Windows proxy settings",
                            "Для приложений с прокси Windows",
                        ),
                        11.0,
                        MUTED,
                    ));
                },
            );
        });
    }

    fn country_choice<'a>(
        &self,
        ui: &mut Ui,
        id: &str,
        current: Option<&'a str>,
        automatic: &str,
        countries: &BTreeSet<&'a str>,
        language: Language,
    ) -> (Response, Option<&'a str>) {
        let label = current.map_or_else(|| automatic.to_owned(), |code| language.country(code));
        let flag = current.filter(|code| REGIONS.contains(code));
        let mut text = egui::text::LayoutJob::default();
        text.append(
            &label,
            if flag.is_some() { 21.0 } else { 0.0 },
            egui::TextFormat {
                font_id: FontId::proportional(12.0),
                color: MUTED,
                ..Default::default()
            },
        );
        let mut option = current;
        let response = ComboBox::from_id_salt(id)
            .width(24.0)
            .height(240.0)
            .selected_text(text)
            .icon(|ui, rect, visuals, open| {
                let p = rect.center();
                let points = if open {
                    [
                        p + vec2(-3.0, -1.5),
                        p + vec2(0.0, 1.5),
                        p + vec2(3.0, -1.5),
                    ]
                } else {
                    [
                        p + vec2(-1.5, -3.0),
                        p + vec2(1.5, 0.0),
                        p + vec2(-1.5, 3.0),
                    ]
                };
                let stroke = Stroke::new(1.2_f32, visuals.fg_stroke.color);
                ui.painter().line_segment([points[0], points[1]], stroke);
                ui.painter().line_segment([points[1], points[2]], stroke);
            })
            .show_ui(ui, |ui| {
                ui.spacing_mut().interact_size.y = 28.0;
                ui.selectable_value(&mut option, None, automatic);
                for &code in countries {
                    ui.selectable_value(&mut option, Some(code), language.country(code));
                }
            })
            .response;
        if let Some(code) = flag {
            self.art
                .flag(ui, response.rect.left_center() + vec2(16.0, 0.0), code);
        }
        (response, option)
    }

    fn protection(
        &self,
        ui: &mut Ui,
        service: Service,
        protection: Protection,
        language: Language,
    ) {
        let (label, color) = match protection {
            Protection::Disabled => return,
            Protection::Blocked => (language.text("Access blocked", "Доступ закрыт"), CORAL),
            Protection::PortUnavailable => (language.text("Port in use", "Порт занят"), CORAL),
            Protection::Verifying => (language.text("Checking country", "Проверяем страну"), AMBER),
            Protection::Allowed => (language.text("Access allowed", "Доступ разрешён"), GREEN),
        };
        ui.horizontal(|ui| {
            ui.label(regular(label, 11.0, color));
            if protection != Protection::PortUnavailable {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let address = format!("127.0.0.1:{}", service.port());
                    if ui.link(regular(&address, 10.5, MUTED)).on_hover_text(language.text(
                        "Copy HTTP proxy address. The country lock applies when the app uses this proxy.",
                        "Скопировать адрес HTTP-прокси. Привязка действует для приложений, которые используют этот прокси.",
                    )).clicked() {
                        ui.ctx().copy_text(address);
                    }
                });
            }
        });
    }

    fn system_proxy(&self, ui: &mut Ui, language: Language) -> Response {
        let selected = self.state.settings.system_proxy;
        let (rect, response) = ui.allocate_exact_size(vec2(41.0, 24.0), Sense::click());
        response.widget_info(|| {
            WidgetInfo::selected(
                WidgetType::Checkbox,
                ui.is_enabled(),
                selected,
                language.text("System proxy", "Системный прокси"),
            )
        });
        if ui.is_rect_visible(rect) {
            let fill = if selected {
                BLUE
            } else if response.hovered() {
                Color32::from_rgb(83, 85, 97)
            } else {
                Color32::from_rgb(70, 72, 83)
            };
            let outline = if response.has_focus() {
                Stroke::new(1.5_f32, BLUE_TEXT)
            } else {
                Stroke::NONE
            };
            ui.painter()
                .rect(rect, 12, fill, outline, StrokeKind::Outside);
            let x = if selected {
                rect.right() - 12.0
            } else {
                rect.left() + 12.0
            };
            ui.painter()
                .circle_filled(pos2(x, rect.center().y), 8.5, Color32::WHITE);
        }
        if response.clicked() {
            self.client.send(Action::SystemProxy(!selected));
        }
        response
    }
}

fn tile(ui: &mut Ui, width: f32, title: &str, body: impl FnOnce(&mut Ui)) {
    Frame::new()
        .fill(SURFACE)
        .corner_radius(13)
        .inner_margin(Margin::symmetric(11, 8))
        .show(ui, |ui| {
            ui.set_width(width - 22.0);
            ui.set_min_height(51.0);
            ui.label(regular(title, 11.0, MUTED));
            ui.add_space(2.0);
            body(ui);
        });
}

fn setting_row(
    ui: &mut Ui,
    title: &str,
    control: impl FnOnce(&mut Ui) -> Response,
    detail: impl FnOnce(&mut Ui),
) {
    Frame::new()
        .inner_margin(Margin::symmetric(11, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let label = ui.label(semibold(title, 13.0, TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    control(ui).labelled_by(label.id)
                });
            });
            detail(ui);
        });
}

fn divider(ui: &mut Ui) {
    let (line, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(
        line.x_range(),
        line.center().y,
        Stroke::new(1.0_f32, DIVIDER),
    );
}

fn uptime(elapsed: Duration, language: Language) -> String {
    let seconds = elapsed.as_secs();
    if seconds >= 3600 {
        format!(
            "{} {} {} {}",
            seconds / 3600,
            language.text("h", "ч"),
            seconds / 60 % 60,
            language.text("min", "мин")
        )
    } else if seconds >= 60 {
        format!("{} {}", seconds / 60, language.text("min", "мин"))
    } else {
        format!("{seconds} {}", language.text("s", "с"))
    }
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
