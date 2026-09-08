//! Client for the local `koi-server` administrative Unix socket.

#[cfg(not(unix))]
fn main() {
    eprintln!("koi-console 当前仅支持 Unix 管理套接字；请在运行 koi-server 的 Linux 主机上执行。");
}

#[cfg(unix)]
mod unix_client {
    use std::io::{self, BufRead, Write};
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::sync::mpsc;

    #[derive(Serialize)]
    struct AdminRequest<'a> {
        command: &'a str,
    }

    #[derive(Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum AdminResponse {
        Ready {
            message: String,
        },
        Output {
            message: String,
        },
        Error {
            message: String,
        },
        Event {
            task_id: String,
            sequence: u64,
            message: String,
        },
        Shutdown,
    }

    pub async fn run() -> Result<(), String> {
        let (socket, command) = parse_args()?;
        let stream = UnixStream::connect(&socket)
            .await
            .map_err(|error| format!("无法连接 {}：{error}", socket.display()))?;
        if let Some(command) = command {
            run_command(stream, &command).await
        } else {
            attach(stream).await
        }
    }

    fn parse_args() -> Result<(PathBuf, Option<String>), String> {
        let mut args = std::env::args().skip(1).collect::<Vec<_>>();
        let mut socket = std::env::var_os("KOI_ADMIN_SOCKET_PATH").map_or_else(
            || PathBuf::from("/run/koi-rust-rv/koi-admin.sock"),
            PathBuf::from,
        );
        if let Some(index) = args.iter().position(|arg| arg == "--socket") {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "--socket 需要路径".to_owned())?
                .clone();
            socket = PathBuf::from(value);
            args.drain(index..=index + 1);
        }
        if args
            .first()
            .is_some_and(|arg| arg == "--help" || arg == "-h")
        {
            return Err("用法：koi-console [--socket PATH] [attach|<命令…>]".into());
        }
        let command = match args.first().map(String::as_str) {
            None | Some("attach") => {
                if args.len() > 1 {
                    return Err("attach 不接受额外参数".into());
                }
                None
            }
            Some(_) => Some(args.join(" ")),
        };
        Ok((socket, command))
    }

    async fn run_command(stream: UnixStream, command: &str) -> Result<(), String> {
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        print_response(read_response(&mut lines).await?)?;
        send_command(&mut writer, command).await?;
        loop {
            let response = read_response(&mut lines).await?;
            let finished = matches!(
                response,
                AdminResponse::Output { .. } | AdminResponse::Error { .. }
            );
            print_response(response)?;
            if finished {
                return Ok(());
            }
        }
    }

    async fn attach(stream: UnixStream) -> Result<(), String> {
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        print_response(read_response(&mut lines).await?)?;
        println!("输入 help 查看命令，Ctrl+C 断开客户端。\n");
        print_prompt();
        let (sender, mut commands) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("koi-console-input".into())
            .spawn(move || read_stdin(&sender))
            .map_err(|error| format!("无法启动终端输入线程：{error}"))?;

        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(line)) => {
                        print_response(parse_response(&line)?)?;
                        print_prompt();
                    }
                    Ok(None) => return Ok(()),
                    Err(error) => return Err(format!("读取服务端消息失败：{error}")),
                },
                command = commands.recv() => if let Some(command) = command {
                    send_command(&mut writer, &command).await?;
                } else {
                    return Ok(());
                }
            }
        }
    }

    async fn send_command(
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        command: &str,
    ) -> Result<(), String> {
        let encoded =
            serde_json::to_string(&AdminRequest { command }).map_err(|error| error.to_string())?;
        writer
            .write_all(encoded.as_bytes())
            .await
            .map_err(|error| error.to_string())?;
        writer
            .write_all(b"\n")
            .await
            .map_err(|error| error.to_string())?;
        writer.flush().await.map_err(|error| error.to_string())
    }

    async fn read_response(
        lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    ) -> Result<AdminResponse, String> {
        let line = lines
            .next_line()
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "服务端在发送响应前关闭连接".to_owned())?;
        parse_response(&line)
    }

    fn parse_response(line: &str) -> Result<AdminResponse, String> {
        serde_json::from_str(line).map_err(|error| format!("服务端响应格式无效：{error}"))
    }

    fn print_response(response: AdminResponse) -> Result<(), String> {
        match response {
            AdminResponse::Ready { message } | AdminResponse::Output { message } => {
                println!("{message}");
            }
            AdminResponse::Error { message } => eprintln!("命令失败：{message}"),
            AdminResponse::Event {
                task_id,
                sequence,
                message,
            } => {
                println!("[event {task_id}#{sequence}] {message}");
            }
            AdminResponse::Shutdown => return Err("服务正在关闭".into()),
        }
        Ok(())
    }

    fn read_stdin(sender: &mpsc::UnboundedSender<String>) {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(line) => {
                    if sender.send(line).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    eprintln!("读取输入失败：{error}");
                    break;
                }
            }
        }
    }

    fn print_prompt() {
        print!("koi-admin> ");
        let _ = io::stdout().flush();
    }
}

#[cfg(unix)]
#[tokio::main]
async fn main() {
    if let Err(error) = unix_client::run().await {
        eprintln!("koi-console: {error}");
        std::process::exit(1);
    }
}
