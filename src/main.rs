// 发布版不弹控制台黑框：双击打开时少一次窗口创建，也避免视觉闪烁。
// debug 版保留控制台，便于开发期直接看日志。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use ngy_image_viewer::{app, open_job, perf, trace};

fn main() -> anyhow::Result<()> {
    // 第一件事：确立时间零点。双击打开场景下，进程启动到这个函数的时间越短越好，
    // 因此这里不做任何与参数解析无关的工作。
    perf::init();
    install_panic_hook();
    perf::schedule_bench_exit();

    let options = app::AppOptions::from_args();
    trace::step(
        "main",
        format!(
            "参数解析完成：路径={:?} 尺寸上限={}×{} 像素上限={} 阶段日志={}",
            options.path.as_deref().map(|path| path.display().to_string()),
            options.limits.max_width,
            options.limits.max_height,
            options.limits.max_pixels,
            trace::enabled(),
        ),
    );

    // 「感知速度优先」的落点：一拿到路径就开跑，不等 GPUI 那几百毫秒的平台初始化。
    //
    // 时序上，解码线程与「application 构建 → init(cx) → GPU 初始化 → 建窗口」完全并行，
    // 于是用户的等待时间接近两者中的较大值，而不是两段之和。
    // 这是本方案里唯一一处「为了速度而刻意提前」的设计，值得单独标一行打点。
    let task = options.path.clone().map(open_job::OpenTask::spawn);
    perf::mark("open_task_spawned");

    if let Err(error) = app::run(options, task) {
        perf::log_always(&format!("[fatal] 应用启动失败: {error:#}"));
    }

    if perf::enabled() {
        perf::log_always(&perf::summary());
    }

    Ok(())
}

/// 把 panic 信息落到日志文件，避免发布版（无控制台）崩溃时毫无线索。
///
/// 产品要求是「绝不崩溃」，但真出了预料之外的问题时，至少要留下可复现的现场：
/// 发布版没有任何标准输出，日志文件是唯一的线索来源。
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info.to_string();
        perf::log_always(&format!("[panic] {message}"));
        previous(info);
    }));
}
