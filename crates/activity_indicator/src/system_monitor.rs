use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    Render, StatefulInteractiveElement as _, Task, WeakEntity, Window, actions, px,
};
use proto::GetSystemStatsResponse;
use std::{collections::VecDeque, time::Duration};
use sysinfo::{Disks, Networks, ProcessesToUpdate, System};
use ui::{ProgressBar, prelude::*};
use workspace::{
    Panel, StatusItemView, Workspace,
    dock::{DockPosition, PanelEvent},
    item::ItemHandle,
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const HISTORY_LENGTH: usize = 30;

pub const SYSTEM_MONITOR_PANEL_KEY: &str = "SystemMonitorPanel";

actions!(system_monitor, [ToggleFocus]);

#[derive(Clone, Default)]
pub struct SystemStats {
    pub hostname: String,
    pub os_name: String,
    pub kernel_version: String,
    pub uptime_seconds: u64,
    pub cpu_usage_percent: f32,
    pub cpu_core_usage_percent: Vec<f32>,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub swap_total_bytes: u64,
    pub disk_used_bytes: u64,
    pub disk_total_bytes: u64,
    pub network_received_bytes_per_second: u64,
    pub network_transmitted_bytes_per_second: u64,
    pub process_count: u32,
    pub load_average: [f64; 3],
}

impl From<GetSystemStatsResponse> for SystemStats {
    fn from(stats: GetSystemStatsResponse) -> Self {
        Self {
            hostname: stats.hostname,
            os_name: stats.os_name,
            kernel_version: stats.kernel_version,
            uptime_seconds: stats.uptime_seconds,
            cpu_usage_percent: stats.cpu_usage_percent,
            cpu_core_usage_percent: stats.cpu_core_usage_percent,
            memory_used_bytes: stats.memory_used_bytes,
            memory_total_bytes: stats.memory_total_bytes,
            swap_used_bytes: stats.swap_used_bytes,
            swap_total_bytes: stats.swap_total_bytes,
            disk_used_bytes: stats.disk_used_bytes,
            disk_total_bytes: stats.disk_total_bytes,
            network_received_bytes_per_second: stats.network_received_bytes_per_second,
            network_transmitted_bytes_per_second: stats.network_transmitted_bytes_per_second,
            process_count: stats.process_count,
            load_average: [
                stats.load_average_one,
                stats.load_average_five,
                stats.load_average_fifteen,
            ],
        }
    }
}

struct LocalSampler {
    system: System,
    disks: Disks,
    networks: Networks,
    last_sample: std::time::Instant,
}

impl LocalSampler {
    fn new() -> Self {
        let mut system = System::new();
        system.refresh_cpu_all();
        system.refresh_memory();
        system.refresh_processes(ProcessesToUpdate::All, true);
        Self {
            system,
            disks: Disks::new_with_refreshed_list(),
            networks: Networks::new_with_refreshed_list(),
            last_sample: std::time::Instant::now(),
        }
    }

    fn sample(&mut self) -> SystemStats {
        let elapsed = self.last_sample.elapsed().as_secs_f64().max(0.001);
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.system.refresh_processes(ProcessesToUpdate::All, true);
        self.disks.refresh(true);
        self.networks.refresh(true);
        self.last_sample = std::time::Instant::now();

        let disk_total_bytes: u64 = self.disks.iter().map(|disk| disk.total_space()).sum();
        let disk_available_bytes: u64 = self.disks.iter().map(|disk| disk.available_space()).sum();
        let load = System::load_average();
        let received: u64 = self.networks.values().map(|data| data.received()).sum();
        let transmitted: u64 = self.networks.values().map(|data| data.transmitted()).sum();

        SystemStats {
            hostname: System::host_name().unwrap_or_else(|| "本机".into()),
            os_name: System::long_os_version()
                .or_else(System::name)
                .unwrap_or_else(|| "未知系统".into()),
            kernel_version: System::kernel_version().unwrap_or_default(),
            uptime_seconds: System::uptime(),
            cpu_usage_percent: self.system.global_cpu_usage(),
            cpu_core_usage_percent: self
                .system
                .cpus()
                .iter()
                .map(|cpu| cpu.cpu_usage())
                .collect(),
            memory_used_bytes: self.system.used_memory(),
            memory_total_bytes: self.system.total_memory(),
            swap_used_bytes: self.system.used_swap(),
            swap_total_bytes: self.system.total_swap(),
            disk_used_bytes: disk_total_bytes.saturating_sub(disk_available_bytes),
            disk_total_bytes,
            network_received_bytes_per_second: (received as f64 / elapsed) as u64,
            network_transmitted_bytes_per_second: (transmitted as f64 / elapsed) as u64,
            process_count: self.system.processes().len().try_into().unwrap_or(u32::MAX),
            load_average: [load.one, load.five, load.fifteen],
        }
    }
}

pub struct SystemMonitor {
    stats: Option<SystemStats>,
    cpu_history: VecDeque<f32>,
    download_history: VecDeque<u64>,
    upload_history: VecDeque<u64>,
    remote_name: Option<String>,
    error: Option<String>,
    _refresh_task: Task<()>,
}

impl SystemMonitor {
    pub fn new(workspace: &Workspace, cx: &mut App) -> Entity<Self> {
        let remote_client = workspace.project().read(cx).remote_client();
        let remote_name = remote_client.as_ref().map(|client| {
            let options = client.read(cx).connection_options();
            match options {
                remote::RemoteConnectionOptions::Ssh(options) => options.host.to_string(),
                remote::RemoteConnectionOptions::Wsl(options) => options.distro_name,
                remote::RemoteConnectionOptions::Docker(options) => options.container_id,
                remote::RemoteConnectionOptions::Mock(_) => "远程主机".into(),
            }
        });
        cx.new(|cx| {
            let refresh_task = cx.spawn({
                let remote_client = remote_client.clone();
                async move |this: WeakEntity<SystemMonitor>, cx| {
                    let mut local_sampler = remote_client.is_none().then(LocalSampler::new);
                    loop {
                        let result = if let Some(remote_client) = remote_client.as_ref() {
                            if remote_client
                                .read_with(cx, |client, _| client.supports_system_stats())
                            {
                                let request =
                                    remote_client.read_with(cx, |client, _| client.system_stats());
                                request.await.map(SystemStats::from)
                            } else {
                                Err(anyhow::anyhow!(
                                    "远程服务不支持系统监控，请选择 Zed CN 远程服务并重新连接"
                                ))
                            }
                        } else if let Some(sampler) = local_sampler.take() {
                            let (sampler, stats) = cx
                                .background_spawn(async move {
                                    let mut sampler = sampler;
                                    let stats = sampler.sample();
                                    (sampler, stats)
                                })
                                .await;
                            local_sampler = Some(sampler);
                            Ok(stats)
                        } else {
                            Err(anyhow::anyhow!("本机监控采样器不可用"))
                        };
                        if this
                            .update(cx, |this, cx| this.apply_sample(result, cx))
                            .is_err()
                        {
                            break;
                        }
                        cx.background_executor().timer(REFRESH_INTERVAL).await;
                    }
                }
            });
            Self {
                stats: None,
                cpu_history: VecDeque::with_capacity(HISTORY_LENGTH),
                download_history: VecDeque::with_capacity(HISTORY_LENGTH),
                upload_history: VecDeque::with_capacity(HISTORY_LENGTH),
                remote_name,
                error: None,
                _refresh_task: refresh_task,
            }
        })
    }

    fn apply_sample(&mut self, result: anyhow::Result<SystemStats>, cx: &mut Context<Self>) {
        match result {
            Ok(stats) => {
                push_history(&mut self.cpu_history, stats.cpu_usage_percent);
                push_history(
                    &mut self.download_history,
                    stats.network_received_bytes_per_second,
                );
                push_history(
                    &mut self.upload_history,
                    stats.network_transmitted_bytes_per_second,
                );
                self.stats = Some(stats);
                self.error = None;
            }
            Err(error) => self.error = Some(format!("{error:#}")),
        }
        cx.notify();
    }

    fn target_label(&self) -> String {
        self.remote_name
            .as_ref()
            .map(|name| format!("远程 · {name}"))
            .unwrap_or_else(|| "本机".into())
    }

    fn tooltip_element(&self) -> gpui::AnyElement {
        let content = if let Some(stats) = self.stats.as_ref() {
            v_flex()
                .w(px(280.))
                .gap_2()
                .child(
                    h_flex()
                        .justify_between()
                        .child(Label::new(format!("{}运行状态", self.target_label())))
                        .child(
                            Label::new(stats.hostname.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .child(metric_line(
                    "CPU",
                    format!("{:.0}%", stats.cpu_usage_percent),
                ))
                .child(metric_line(
                    "内存",
                    format!(
                        "{} / {}",
                        format_bytes(stats.memory_used_bytes),
                        format_bytes(stats.memory_total_bytes)
                    ),
                ))
                .child(metric_line(
                    "磁盘",
                    format!(
                        "{} / {}",
                        format_bytes(stats.disk_used_bytes),
                        format_bytes(stats.disk_total_bytes)
                    ),
                ))
                .child(metric_line(
                    "网络",
                    format!(
                        "↓ {}/s  ↑ {}/s",
                        format_bytes(stats.network_received_bytes_per_second),
                        format_bytes(stats.network_transmitted_bytes_per_second)
                    ),
                ))
                .child(
                    Label::new("点击打开右侧系统监控")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else {
            v_flex()
                .w(px(280.))
                .gap_1()
                .child(Label::new(format!("{}运行状态", self.target_label())))
                .child(
                    Label::new(
                        self.error
                            .clone()
                            .unwrap_or_else(|| "正在读取系统状态…".into()),
                    )
                    .size(LabelSize::Small)
                    .color(if self.error.is_some() {
                        Color::Error
                    } else {
                        Color::Muted
                    }),
                )
                .into_any_element()
        };
        content
    }
}

impl Render for SystemMonitor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let monitor = cx.entity();
        IconButton::new("system-monitor-status", IconName::Gauge)
            .icon_size(IconSize::Small)
            .tooltip(ui::Tooltip::element(move |_, cx| {
                monitor.read(cx).tooltip_element()
            }))
            .on_click(|_, window, cx| {
                window.dispatch_action(ToggleFocus.boxed_clone(), cx);
            })
    }
}

impl StatusItemView for SystemMonitor {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

pub struct SystemMonitorPanel {
    monitor: Entity<SystemMonitor>,
    focus_handle: FocusHandle,
}

impl SystemMonitorPanel {
    pub fn new(monitor: Entity<SystemMonitor>, cx: &mut Context<Self>) -> Self {
        Self {
            monitor,
            focus_handle: cx.focus_handle(),
        }
    }
}

impl EventEmitter<PanelEvent> for SystemMonitorPanel {}

impl Focusable for SystemMonitorPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for SystemMonitorPanel {
    fn persistent_name() -> &'static str {
        "System Monitor"
    }

    fn panel_key() -> &'static str {
        SYSTEM_MONITOR_PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Right
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> gpui::Pixels {
        px(340.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Gauge)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("系统监控")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        7
    }
}

impl Render for SystemMonitorPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let monitor = self.monitor.read(cx);
        let stats = monitor.stats.as_ref();
        v_flex()
            .id("system-monitor-panel")
            .size_full()
            .track_focus(&self.focus_handle)
            .overflow_y_scroll()
            .p_3()
            .gap_3()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(IconName::Gauge).color(Color::Accent))
                            .child(Label::new("系统监控")),
                    )
                    .child(
                        Label::new(monitor.target_label())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .when_some(stats, |element, stats| {
                element
                    .child(system_card(stats))
                    .child(resource_card(
                        "CPU",
                        IconName::Gauge,
                        stats.cpu_usage_percent,
                        format!("{:.1}%", stats.cpu_usage_percent),
                        Some(history_bars(&monitor.cpu_history, 100.)),
                        cx,
                    ))
                    .child(resource_card(
                        "内存",
                        IconName::DatabaseZap,
                        percentage(stats.memory_used_bytes, stats.memory_total_bytes),
                        format!(
                            "{} / {}",
                            format_bytes(stats.memory_used_bytes),
                            format_bytes(stats.memory_total_bytes)
                        ),
                        None,
                        cx,
                    ))
                    .child(network_card(stats, monitor, cx))
                    .child(resource_card(
                        "磁盘",
                        IconName::Server,
                        percentage(stats.disk_used_bytes, stats.disk_total_bytes),
                        format!(
                            "{} / {}",
                            format_bytes(stats.disk_used_bytes),
                            format_bytes(stats.disk_total_bytes)
                        ),
                        None,
                        cx,
                    ))
            })
            .when(stats.is_none(), |element| {
                element.child(
                    v_flex()
                        .p_3()
                        .gap_2()
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .rounded_md()
                        .child(Label::new(
                            monitor
                                .error
                                .clone()
                                .unwrap_or_else(|| "正在读取系统状态…".into()),
                        )),
                )
            })
    }
}

fn system_card(stats: &SystemStats) -> impl IntoElement {
    v_flex()
        .p_3()
        .gap_2()
        .border_1()
        .border_color(gpui::transparent_black())
        .rounded_md()
        .bg(gpui::transparent_black().opacity(0.08))
        .child(
            h_flex()
                .justify_between()
                .child(Label::new(stats.hostname.clone()))
                .child(
                    Label::new(format_uptime(stats.uptime_seconds))
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                ),
        )
        .child(
            Label::new(stats.os_name.clone())
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .when(!stats.kernel_version.is_empty(), |element| {
            element.child(
                Label::new(format!("内核 {}", stats.kernel_version))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
        })
        .child(
            Label::new(format!(
                "进程 {} · 负载 {:.2}  {:.2}  {:.2}",
                stats.process_count,
                stats.load_average[0],
                stats.load_average[1],
                stats.load_average[2]
            ))
            .size(LabelSize::Small)
            .color(Color::Muted),
        )
}

fn resource_card(
    title: &'static str,
    icon: IconName,
    percent: f32,
    value: String,
    history: Option<gpui::AnyElement>,
    cx: &App,
) -> impl IntoElement {
    v_flex()
        .p_3()
        .gap_2()
        .border_1()
        .border_color(cx.theme().colors().border)
        .rounded_md()
        .child(
            h_flex()
                .justify_between()
                .child(
                    h_flex()
                        .gap_2()
                        .child(Icon::new(icon).size(IconSize::Small).color(Color::Accent))
                        .child(Label::new(title)),
                )
                .child(Label::new(value).size(LabelSize::Small)),
        )
        .when_some(history, |element, history| element.child(history))
        .child(ProgressBar::new(title, percent, 100., cx))
}

fn network_card(stats: &SystemStats, monitor: &SystemMonitor, cx: &App) -> impl IntoElement {
    let max_rate = monitor
        .download_history
        .iter()
        .chain(monitor.upload_history.iter())
        .copied()
        .max()
        .unwrap_or(1)
        .max(1) as f32;
    v_flex()
        .p_3()
        .gap_2()
        .border_1()
        .border_color(cx.theme().colors().border)
        .rounded_md()
        .child(
            h_flex()
                .gap_2()
                .child(
                    Icon::new(IconName::Public)
                        .size(IconSize::Small)
                        .color(Color::Accent),
                )
                .child(Label::new("网络")),
        )
        .child(history_bars_u64(&monitor.download_history, max_rate))
        .child(metric_line(
            "下载",
            format!(
                "{}/s",
                format_bytes(stats.network_received_bytes_per_second)
            ),
        ))
        .child(metric_line(
            "上传",
            format!(
                "{}/s",
                format_bytes(stats.network_transmitted_bytes_per_second)
            ),
        ))
}

fn history_bars(values: &VecDeque<f32>, maximum: f32) -> gpui::AnyElement {
    h_flex()
        .h_8()
        .items_end()
        .gap_px()
        .children(values.iter().enumerate().map(|(index, value)| {
            div()
                .id(("history", index))
                .w_1()
                .h(relative((*value / maximum).clamp(0.04, 1.)))
                .bg(gpui::blue())
        }))
        .into_any_element()
}

fn history_bars_u64(values: &VecDeque<u64>, maximum: f32) -> gpui::AnyElement {
    let values = values.iter().map(|value| *value as f32).collect();
    history_bars(&values, maximum)
}

fn metric_line(label: &'static str, value: String) -> impl IntoElement {
    h_flex()
        .justify_between()
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
        .child(Label::new(value).size(LabelSize::Small))
}

fn push_history<T>(history: &mut VecDeque<T>, value: T) {
    if history.len() == HISTORY_LENGTH {
        history.pop_front();
    }
    history.push_back(value);
}

fn percentage(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.
    } else {
        used as f32 / total as f32 * 100.
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.;
    const MIB: f64 = KIB * 1024.;
    const GIB: f64 = MIB * 1024.;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{} B", bytes as u64)
    }
}

fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    if days > 0 {
        format!("已运行 {days} 天 {hours} 小时")
    } else if hours > 0 {
        format!("已运行 {hours} 小时 {minutes} 分")
    } else {
        format!("已运行 {minutes} 分钟")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_uptime_and_bytes_for_compact_surfaces() {
        assert_eq!(format_uptime(90), "已运行 1 分钟");
        assert_eq!(format_uptime(3_900), "已运行 1 小时 5 分");
        assert_eq!(format_uptime(180_000), "已运行 2 天 2 小时");
        assert_eq!(format_bytes(1_536), "1.5 KiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }

    #[test]
    fn history_stays_bounded() {
        let mut history = VecDeque::new();
        for value in 0..HISTORY_LENGTH + 5 {
            push_history(&mut history, value);
        }
        assert_eq!(history.len(), HISTORY_LENGTH);
        assert_eq!(history.front(), Some(&5));
        assert_eq!(history.back(), Some(&(HISTORY_LENGTH + 4)));
    }

    #[test]
    fn maps_remote_statistics_without_losing_fields() {
        let stats = SystemStats::from(GetSystemStatsResponse {
            hostname: "dev-host".into(),
            os_name: "Linux".into(),
            kernel_version: "6.8".into(),
            uptime_seconds: 42,
            cpu_usage_percent: 37.5,
            cpu_core_usage_percent: vec![25., 50.],
            memory_used_bytes: 4,
            memory_total_bytes: 8,
            swap_used_bytes: 1,
            swap_total_bytes: 2,
            disk_used_bytes: 10,
            disk_total_bytes: 20,
            network_received_bytes_per_second: 30,
            network_transmitted_bytes_per_second: 40,
            process_count: 50,
            load_average_one: 0.5,
            load_average_five: 0.25,
            load_average_fifteen: 0.125,
            sampled_at_unix_seconds: 60,
        });
        assert_eq!(stats.hostname, "dev-host");
        assert_eq!(stats.cpu_core_usage_percent, [25., 50.]);
        assert_eq!(stats.load_average, [0.5, 0.25, 0.125]);
        assert_eq!(stats.network_transmitted_bytes_per_second, 40);
    }
}
