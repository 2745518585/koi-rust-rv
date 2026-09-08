//! Unix-domain-socket administration endpoint for service deployments.
//!
//! It intentionally never opens a TCP port. Socket ownership and filesystem permissions are the
//! authentication boundary; every accepted command is still persisted through the normal system
//! control path.

use std::path::PathBuf;
use std::sync::Arc;

use koi_infra::event_store::JsonlEventStore;
use koi_infra::llm::ModelProviderRegistry;
use tokio_util::sync::CancellationToken;

use crate::agent_runtime::AgentSupervisor;

#[cfg(unix)]
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

#[cfg(unix)]
use crate::console::{ConsoleOutcome, TerminalConsole};

/// Starts the local administrative socket listener.
pub fn spawn(
    path: PathBuf,
    store: Arc<JsonlEventStore>,
    models: Arc<ModelProviderRegistry>,
    supervisor: Arc<AgentSupervisor>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    #[cfg(unix)]
    {
        tokio::spawn(async move {
            if let Err(error) = serve(path, store, models, supervisor, shutdown).await {
                tracing::error!(%error, "本地管理套接字已停止");
            }
        })
    }

    #[cfg(not(unix))]
    {
        let _ = (path, store, models, supervisor, shutdown);
        tokio::spawn(async {
            tracing::warn!("当前平台不支持 Unix 管理套接字；请使用进程内 --console 控制台");
        })
    }
}

#[cfg(unix)]
#[derive(Deserialize)]
struct AdminRequest {
    command: String,
}

#[cfg(unix)]
#[derive(Serialize)]
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

#[cfg(unix)]
async fn serve(
    path: PathBuf,
    store: Arc<JsonlEventStore>,
    models: Arc<ModelProviderRegistry>,
    supervisor: Arc<AgentSupervisor>,
    shutdown: CancellationToken,
) -> Result<(), String> {
    prepare_socket_path(&path)?;
    let listener = UnixListener::bind(&path)
        .map_err(|error| format!("绑定 {} 失败：{error}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))
        .map_err(|error| format!("设置 {} 权限失败：{error}", path.display()))?;
    tracing::info!(path = %path.display(), "本地管理套接字已启用");

    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _address)) => {
                    let connection_store = Arc::clone(&store);
                    let connection_models = Arc::clone(&models);
                    let connection_supervisor = Arc::clone(&supervisor);
                    let connection_shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        if let Err(error) = handle_connection(
                            stream,
                            connection_store,
                            connection_models,
                            connection_supervisor,
                            connection_shutdown,
                        ).await {
                            tracing::debug!(%error, "本地管理客户端已断开");
                        }
                    });
                }
                Err(error) => tracing::warn!(%error, "接受本地管理客户端失败"),
            }
        }
    }

    if let Err(error) = std::fs::remove_file(&path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(%error, path = %path.display(), "清理本地管理套接字失败");
        }
    }
    Ok(())
}

#[cfg(unix)]
fn prepare_socket_path(path: &std::path::Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("管理套接字没有父目录：{}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("创建管理套接字目录 {} 失败：{error}", parent.display()))?;
    if !path.exists() {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("读取管理套接字 {} 失败：{error}", path.display()))?;
    if !metadata.file_type().is_socket() {
        return Err(format!("拒绝覆盖非套接字文件：{}", path.display()));
    }
    std::fs::remove_file(path)
        .map_err(|error| format!("移除遗留管理套接字 {} 失败：{error}", path.display()))
}

#[cfg(unix)]
async fn handle_connection(
    stream: UnixStream,
    store: Arc<JsonlEventStore>,
    models: Arc<ModelProviderRegistry>,
    supervisor: Arc<AgentSupervisor>,
    shutdown: CancellationToken,
) -> Result<(), String> {
    let console = TerminalConsole::new(store.clone(), models, supervisor, shutdown.clone());
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut events = store.subscribe();
    write_response(
        &mut writer,
        &AdminResponse::Ready {
            message: "已连接本地 Koi 管理控制台；输入 help 查看命令。".into(),
        },
    )
    .await?;

    loop {
        tokio::select! {
            () = shutdown.cancelled() => {
                write_response(&mut writer, &AdminResponse::Shutdown).await?;
                break;
            }
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    let request = serde_json::from_str::<AdminRequest>(&line)
                        .map_err(|error| format!("管理请求格式无效：{error}"))?;
                    match console.execute(&request.command).await {
                        Ok(ConsoleOutcome::Continue(message)) => {
                            write_response(&mut writer, &AdminResponse::Output { message }).await?;
                        }
                        Ok(ConsoleOutcome::Shutdown) => {
                            write_response(&mut writer, &AdminResponse::Output {
                                message: "正在关闭 koi-server…".into(),
                            }).await?;
                        }
                        Err(error) => {
                            write_response(&mut writer, &AdminResponse::Error { message: error }).await?;
                        }
                    }
                }
                Ok(None) => break,
                Err(error) => return Err(format!("读取管理请求失败：{error}")),
            },
            event = events.recv() => match event {
                Ok(event) => {
                    write_response(&mut writer, &AdminResponse::Event {
                        task_id: event.task_id.to_string(),
                        sequence: event.sequence,
                        message: shorten(&format!("{:?}", event.payload), 2_000),
                    }).await?;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    write_response(&mut writer, &AdminResponse::Output {
                        message: format!("管理事件流落后，已跳过 {count} 条事件"),
                    }).await?;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
async fn write_response<W>(writer: &mut W, response: &AdminResponse) -> Result<(), String>
where
    W: AsyncWrite + Unpin,
{
    let encoded = serde_json::to_string(response).map_err(|error| error.to_string())?;
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

#[cfg(unix)]
fn shorten(value: &str, limit: usize) -> String {
    let mut result = value.chars().take(limit).collect::<String>();
    if value.chars().count() > limit {
        result.push_str("…");
    }
    result
}
