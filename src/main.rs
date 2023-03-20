use chrono::Timelike;
use serde::{Deserialize, Serialize};
use shared_child::SharedChild;
use std::{
    env,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};
use timer::Timer;
use tokio::{
    fs::File,
    io::{stdin, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    join, sync,
};
use tuples::{TupleCloned, TupleTransposeResultSameError};

const CONFIG_FILE_NAME: &str = "restarter.json";

#[tokio::main]
async fn main() -> tokio::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    let cwd = env::current_dir()?;
    let config_path = cwd.join(CONFIG_FILE_NAME);
    let config = if config_path.exists() {
        load_config(&config_path).await?
    } else {
        make_config(&config_path).await?
    };
    let (tx, _) = sync::broadcast::channel::<Oper>(16);
    let tx = Arc::new(tx);
    let tx2 = tx.clone();

    let timer = Box::leak(Box::new(Timer::new()));

    match config.mode {
        Mode::Daily { time } => {
            let now = chrono::Local::now()
                .with_hour(0)
                .unwrap()
                .with_minute(0)
                .unwrap()
                .with_second(0)
                .unwrap()
                .with_nanosecond(0)
                .unwrap();
            timer
                .schedule(
                    now,
                    Some(chrono::Duration::from_std(time).unwrap()),
                    move || {
                        log::info!("定时器触发");
                        match tx.send(Oper::Stop) {
                            Ok(_) => {}
                            Err(e) => {
                                log::error!("发送定时器信号失败，错误：{}", e);
                            }
                        }
                    },
                )
                .ignore();
        }
    }

    loop {
        let tx = &tx2;
        let process = tokio::spawn(start_process(config.clone(), tx.subscribe()));

        join!(process)
            .transpose_same_error()?
            .transpose_same_error()?;

        log::info!("等待 10 秒");
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Config {
    pub path: PathBuf,
    pub stop_command: Option<String>,
    pub timeout: Option<Duration>,
    pub mode: Mode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    // 每日
    Daily { time: Duration },
    // // 间隔
    // Interval {
    //     duration: Duration,
    // },
    // // 相对间隔
    // RelativeInterval {
    //     time: DateTime<FixedOffset>,
    //     duration: Duration,
    // },
}

impl Default for Mode {
    fn default() -> Self {
        Self::Daily {
            time: Duration::default(),
        }
    }
}

async fn make_config(path: &Path) -> tokio::io::Result<Config> {
    let mut config: Config = Default::default();
    println!("第一次启动，请填写配置");
    println!("");
    let mut lines = BufReader::new(stdin()).lines();
    loop {
        println!("请填写要启动的路径：");
        let line = lines.next_line().await?;
        if line.as_ref().map(|l| l.is_empty()).unwrap_or(true) {
            continue;
        }
        config.path = PathBuf::from(line.unwrap());
        break;
    }
    println!("请填写停止命令（留空强制结束进程）：");
    let line = lines.next_line().await?;
    config.stop_command = line.map(|l| l.trim().to_string());
    if !config
        .stop_command
        .as_ref()
        .map(|s| s.is_empty())
        .unwrap_or(true)
    {
        loop {
            println!("请输入停止命令超时时间（分钟，支持小数，留空将无限等待）：");
            let line = lines.next_line().await?;
            if line.as_ref().map(|l| l.is_empty()).unwrap_or(true) {
                break;
            }
            let s: f64 = match line.unwrap().parse() {
                Ok(v) => v,
                Err(_) => {
                    println!("格式错误");
                    continue;
                }
            };
            config.timeout = if s < 0.0 {
                None
            } else {
                Some(Duration::from_secs_f64(s * 60.0))
            };
            break;
        }
    }
    #[allow(unused_labels)]
    'm: loop {
        println!("请选择重启模式 （暂时只有1, 输入数字）：");
        println!("1.每日 (每天固定时间点执行)");
        // println!("2.间隔 (以第一次启动的时间为基准，固定间隔执行)");
        // println!("3.相对间隔 (以一个指定的时间为基准，固定间隔执行)");
        let line = lines.next_line().await?;
        let n: u8 = match line.map(|l| l.parse()).transpose() {
            Ok(Some(v)) => v,
            Ok(None) | Err(_) => {
                continue;
            }
        };
        let mode: Mode = match n {
            1 => loop {
                println!("每日：请输入时间（小时，支持小数，输入 back 重新选模式）：");
                let line = lines.next_line().await?;
                if line.as_ref().map(|l| l.is_empty()).unwrap_or(true) {
                    continue;
                }
                let h: f64 = match line.unwrap().parse() {
                    Ok(v) => v,
                    Err(_) => {
                        println!("格式错误");
                        continue;
                    }
                };
                if h < 0.0 {
                    println!("不能小于 0");
                    continue;
                }
                let time = Duration::from_secs_f64(h * 60.0 * 60.0);
                break Mode::Daily { time };
            },
            // 2 => todo!(),
            // 3 => todo!(),
            _ => continue,
        };
        config.mode = mode;
        break;
    }
    let mut file = File::create(path).await?;
    let mut json = serde_json::to_vec_pretty(&config)?;
    file.write_all(&mut json).await?;
    file.flush().await?;
    Ok(config)
}

async fn load_config(path: &Path) -> tokio::io::Result<Config> {
    let mut file = File::open(path).await?;
    let mut json = Vec::new();
    file.read_to_end(&mut json).await?;
    let config = serde_json::from_slice(&json)?;
    Ok(config)
}

#[derive(Debug, Clone, Copy)]
enum Oper {
    Stop,
}

async fn start_process(
    config: Config,
    mut opres: sync::broadcast::Receiver<Oper>,
) -> tokio::io::Result<()> {
    let mut command = Command::new(&config.path);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let child = Arc::new(SharedChild::spawn(&mut command)?);
    let stdin = child.take_stdin();
    let has_stdin = stdin.is_some();

    log::info!("进程开始");

    let (child_exit_tx, mut child_exit_rx) = sync::broadcast::channel::<()>(16);
    let child_exit_tx = Arc::new(child_exit_tx);
    let child_exit_tx2 = child_exit_tx.clone();
    let child_exit_tx3 = child_exit_tx.clone();
    let child_exit_tx4 = child_exit_tx.clone();

    let wait_child = child.clone();
    let wait_child = tokio::task::spawn_blocking(move || {
        match wait_child.wait() {
            Ok(e) => {
                log::warn!("进程退出，返回码： {}", e.code().unwrap_or(0));
            }
            Err(e) => {
                log::error!("进程退出，错误：{}", e);
            }
        }
        tokio::spawn(async move {
            child_exit_tx.send(()).expect("内部错误：发送进程退出信号");
        });
    });

    let (self_stdin_tx, mut self_stdin_rx) = sync::mpsc::channel::<String>(16);
    let self_stdin_tx = Arc::new(self_stdin_tx);
    let self_stdin_tx2 = self_stdin_tx.clone();

    let wait_stdin = tokio::task::spawn(async move {
        let self_stdin_tx = self_stdin_tx2;
        let mut child_exit_rx = child_exit_tx3.subscribe();
        let mut self_stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        loop {
            tokio::select! {
                _ = child_exit_rx.recv() => {
                    return;
                }
                line = self_stdin.next_line() => {
                    match line {
                        Ok(Some(line)) => {
                            self_stdin_tx.send(line).await.expect("内部错误：转发标准输入失败");
                        }
                        Ok(None) => {}
                        Err(e) => {
                            log::error!("读取标准输入出错，错误：{}", e);
                        }
                    }
                }
            }
        }
    });

    let wait_send_stdin = tokio::task::spawn(async move {
        if let Some(mut stdin) = stdin {
            let mut child_exit_rx = child_exit_tx4.subscribe();
            loop {
                tokio::select! {
                    _ = child_exit_rx.recv() => {
                        return;
                    }
                    line = self_stdin_rx.recv() => {
                        match line {
                            None => {}
                            Some(line) => {
                                match stdin.write_all(line.as_bytes()) {
                                    Ok(_) => {},
                                    Err(e) => {
                                        log::error!("转发标准输入失败，错误：{}", e);
                                    },
                                };
                                match stdin.write_all("\n".as_bytes()) {
                                    Ok(_) => {},
                                    Err(e) => {
                                        log::error!("转发标准输入失败，错误：{}", e);
                                    },
                                };
                            }
                        }
                    }
                }
            }
        }
    });

    fn wait_oper_select_kill(child: &Arc<SharedChild>) {
        match child.kill() {
            Ok(_) => {}
            Err(e) => {
                log::error!("杀进程失败，错误：{}", e);
            }
        }
    }

    async fn wait_oper_select_oper(
        child: &Arc<SharedChild>,
        oper: &Oper,
        has_stdin: bool,
        stop_command: &Option<String>,
        timeout: &Option<Duration>,
        time_out_tx: &Arc<sync::mpsc::Sender<()>>,
        child_exit_tx: &Arc<sync::broadcast::Sender<()>>,
        self_stdin_tx: &Arc<sync::mpsc::Sender<String>>,
    ) -> bool {
        match oper {
            Oper::Stop => {
                if let (true, Some(stop_command)) = (has_stdin, stop_command) {
                    if !stop_command.is_empty() {
                        self_stdin_tx
                            .send(stop_command.clone())
                            .await
                            .expect("内部错误：发送退出命令失败");
                        if let Some(timeout) = timeout.cloned() {
                            let time_out_tx = time_out_tx.clone();
                            let mut child_exit_rx = child_exit_tx.subscribe();
                            tokio::spawn(async move {
                                tokio::select! {
                                    _ = tokio::time::sleep(timeout) => {
                                        time_out_tx.send(()).await.expect("内部错误：发送超时信号");
                                    }
                                    _ = child_exit_rx.recv() => {}
                                };
                            });
                        }
                        return false;
                    }
                }
                wait_oper_select_kill(child);
                return true;
            }
        }
    }
    let wait_oper = tokio::spawn(async move {
        let stop_command = &config.stop_command;
        let (time_out_tx, mut time_out_rx) = sync::mpsc::channel::<()>(16);
        let time_out_tx = Arc::new(time_out_tx);
        let child_exit_tx = child_exit_tx2;
        loop {
            tokio::select! {
                _ = child_exit_rx.recv() => {
                    return;
                }
                oper = opres.recv() => {
                    if wait_oper_select_oper(&child, &oper.unwrap(), has_stdin, &stop_command, &config.timeout, &time_out_tx, &child_exit_tx, &self_stdin_tx).await {
                        return;
                    }
                }
                _ = time_out_rx.recv() => {
                    wait_oper_select_kill(&child);
                    return;
                }
            };
        }
    });

    join!(wait_child, wait_stdin, wait_send_stdin, wait_oper).transpose_same_error()?;

    Ok(())
}
