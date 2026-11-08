use std::{
    env,
    error::Error,
    io,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use log::{error, info};
use rusty_ytdl::{Video, VideoOptions, VideoQuality, VideoSearchOptions};
use whatsapp_rust::{
    media::{document_message, DocumentOptions},
    pair_code::PairCodeOptions,
    prelude::*,
    upload::UploadOptions,
    wacore::download::MediaType,
};

const MAX_FILE_SIZE: u64 = 1000 * 1024 * 1024;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    if std::path::Path::new(".env").exists() {
        dotenvy::from_path(".env")?;
    }

    let phone_raw = env::var("WHATSAPP_PHONE_NUMBER").map_err(|_| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "WHATSAPP_PHONE_NUMBER is missing; set it in .env.",
        )
    })?;
    let phone_number = normalize_phone(&phone_raw)?;
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let store = SqliteStore::new("whatsapp_bot.db").await?;
    let bot = Bot::builder()
        .with_backend(store)
        .with_pair_code(PairCodeOptions {
            phone_number,
            ..Default::default()
        })
        .on_pair_code(|code, timeout| async move {
            println!(
                "\nPairing code (valid for {} seconds):\n\n  {}\n",
                timeout.as_secs(),
                code
            );
            println!(
                "On your phone: WhatsApp > Linked devices > Link a device > Link with phone number instead."
            );
        })
        .on_pair_code_error(|err, _client| async move {
            if let Some(rejection) = err.rejection {
                error!("Pairing-code request rejected: {rejection:?}");
            } else {
                error!("Pairing-code request failed locally or timed out.");
            }
            if let Some(backoff) = err.backoff {
                error!("Server retry delay: {} seconds.", backoff.as_secs());
            }
        })
        .on_connected(|_client| async {
            info!("WhatsApp connected; the bot is ready.");
        })
        .on_logged_out(|_info| async {
            error!("WhatsApp device was logged out. Pair it again.");
        })
        .on_message(|ctx| async move {
            let Some(text) = ctx.message.text_content() else {
                return;
            };
            let command = text.trim();

            if let Some(args) = command.strip_prefix("/sendfile ") {
                if let Err(err) = send_file(&ctx, args.trim()).await {
                    error!("Failed to send file: {err}");
                    if let Err(reply_err) = ctx.reply(format!("تعذر إرسال الملف: {err}")).await {
                        error!("Failed to send file error reply: {reply_err}");
                    }
                }
                return;
            }

            if let Some(args) = command.strip_prefix("/yt ") {
                if let Err(err) = download_youtube(&ctx, args.trim(), false).await {
                    error!("Failed to download YouTube video: {err}");
                    if let Err(reply_err) = ctx.reply(format!("تعذر تنزيل الفيديو: {err}")).await {
                        error!("Failed to send YouTube error reply: {reply_err}");
                    }
                }
                return;
            }

            if let Some(args) = command.strip_prefix("/ytaudio ") {
                if let Err(err) = download_youtube(&ctx, args.trim(), true).await {
                    error!("Failed to download YouTube audio: {err}");
                    if let Err(reply_err) = ctx.reply(format!("تعذر تنزيل الصوت: {err}")).await {
                        error!("Failed to send YouTube audio error reply: {reply_err}");
                    }
                }
                return;
            }

            if let Some(response) = response_for_text(command)
                && let Err(err) = ctx.reply(response).await
            {
                error!("Failed to send a reply: {err}");
            }
        })
        .build()
        .await?;

    info!("Starting WhatsApp bot. Session data is stored in whatsapp_bot.db.");
    let mut handle = bot.spawn();
    tokio::select! {
        reason = &mut handle => info!("Bot stopped: {reason:?}"),
        signal = tokio::signal::ctrl_c() => {
            signal?;
            info!("Shutdown signal received; closing gracefully.");
            let report = handle.shutdown().await;
            info!("Shutdown complete: {report:?}");
        }
    }
    Ok(())
}

fn response_for_text(text: &str) -> Option<String> {
    match text {
        "/hi" => Some("HELOO".to_owned()),
        "/مساعدة" | "/help" | "!مساعدة" | "!help" => {
            Some("أوامر البوت:\n/hi\n/ping\n/echo <نص>\n/yt <رابط يوتيوب> — فيديو\n/ytaudio <رابط يوتيوب> — صوت فقط\n/sendfile <المسار> [تعليق]".to_owned())
        }
        "/ping" | "!ping" => Some("Pong — البوت يعمل.".to_owned()),
        _ if text.starts_with("/echo ") => Some(text[6..].trim().to_owned()),
        _ => None,
    }
}

async fn download_youtube(
    ctx: &MessageContext,
    url: &str,
    audio_only: bool,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if url.is_empty() {
        return Err(if audio_only {
            "الاستخدام: /ytaudio <رابط يوتيوب>".into()
        } else {
            "الاستخدام: /yt <رابط يوتيوب>".into()
        });
    }

    ctx.reply(if audio_only {
        "جارٍ تجهيز الصوت من يوتيوب..."
    } else {
        "جارٍ تجهيز الفيديو من يوتيوب..."
    })
    .await?;

    let options = VideoOptions {
        quality: VideoQuality::Lowest,
        filter: if audio_only {
            VideoSearchOptions::Audio
        } else {
            VideoSearchOptions::Video
        },
        ..Default::default()
    };
    let video = Video::new_with_options(url.to_owned(), options)?;
    let info = video.get_info().await?;
    let title = sanitize_filename(&info.video_details.title);
    let extension = if audio_only { "m4a" } else { "mp4" };
    let file_name = format!("{title}-{}.{}", video.get_video_id(), extension);
    let path = temporary_download_path(&file_name);

    let result = async {
        video.download(&path).await?;
        let metadata = tokio::fs::metadata(&path).await?;
        if metadata.len() > MAX_FILE_SIZE {
            return Err::<(), Box<dyn Error + Send + Sync>>(
                "حجم الملف الناتج يتجاوز الحد المسموح (1000 MB)".into(),
            );
        }
        send_file(ctx, &format!("{} تنزيل من يوتيوب", path.display())).await?;
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    }
    .await;

    if let Err(err) = tokio::fs::remove_file(&path).await {
        if err.kind() != io::ErrorKind::NotFound {
            error!("Failed to remove temporary download {}: {err}", path.display());
        }
    }
    result
}

fn temporary_download_path(file_name: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    env::temp_dir().join(format!("whatsapp-ytdl-{timestamp}-{file_name}"))
}

fn sanitize_filename(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            ch if ch.is_control() || ch.is_whitespace() => '_',
            ch => ch,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    let result = if trimmed.is_empty() { "youtube-download" } else { trimmed };
    result.chars().take(100).collect()
}

async fn send_file(ctx: &MessageContext, args: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut parts = args.splitn(2, char::is_whitespace);
    let path_raw = parts.next().unwrap_or_default().trim();
    let caption = parts
        .next()
        .map(str::trim)
        .filter(|caption| !caption.is_empty())
        .map(str::to_owned);

    if path_raw.is_empty() {
        return Err("الاستخدام: /sendfile <المسار> [تعليق]".into());
    }

    let path = Path::new(path_raw);
    let metadata = tokio::fs::metadata(path).await?;
    if !metadata.is_file() {
        return Err("المسار لا يشير إلى ملف عادي".into());
    }
    if metadata.len() > MAX_FILE_SIZE {
        return Err("حجم الملف يتجاوز الحد المسموح (1000 MB)".into());
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("تعذر تحديد اسم الملف")?
        .to_owned();
    let mimetype = mimetype_for_path(path);
    let data = tokio::fs::read(path).await?;
    let upload = ctx
        .client
        .upload(data, MediaType::Document, UploadOptions::default())
        .await?;
    let message = document_message(
        upload,
        DocumentOptions {
            mimetype: Some(mimetype.to_owned()),
            file_name: Some(file_name),
            caption,
            ..Default::default()
        },
    );

    ctx.send_message(message).await?;
    info!("File sent successfully: {}", path.display());
    Ok(())
}

fn mimetype_for_path(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "zip" => "application/zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}

fn normalize_phone(raw: &str) -> Result<String, io::Error> {
    if raw
        .chars()
        .any(|ch| !(ch.is_ascii_digit() || matches!(ch, '+' | '-' | '(' | ')' | ' ')))
    {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "WHATSAPP_PHONE_NUMBER must contain a phone number only.",
        ));
    }
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    if digits.len() < 7 {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "WHATSAPP_PHONE_NUMBER is too short.",
        ));
    }
    Ok(digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_is_normalized() {
        assert_eq!(normalize_phone("+1 (555) 123-4567").unwrap(), "15551234567");
    }

    #[test]
    fn invalid_phone_is_rejected() {
        assert!(normalize_phone("abc").is_err());
    }

    #[test]
    fn bot_commands_are_supported() {
        assert_eq!(response_for_text("/ping").as_deref(), Some("Pong — البوت يعمل."));
        assert_eq!(response_for_text("/echo hello").as_deref(), Some("hello"));
        assert!(response_for_text("/yt https://youtube.com/watch?v=abc").is_none());
    }
}
