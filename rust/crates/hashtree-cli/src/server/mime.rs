/// Get MIME type from filename extension
pub fn get_mime_type(filename: &str) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        // Images
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        // Text
        "txt" => "text/plain; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "xml" => "application/xml",
        "md" => "text/markdown; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        // Documents
        "pdf" => "application/pdf",
        // Video
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "ogg" => "video/ogg",
        // Audio
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        // Archives
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        // Source code
        "rs" => "text/plain; charset=utf-8",
        "py" => "text/plain; charset=utf-8",
        "c" | "h" => "text/plain; charset=utf-8",
        "cpp" | "cc" | "cxx" => "text/plain; charset=utf-8",
        "go" => "text/plain; charset=utf-8",
        "java" => "text/plain; charset=utf-8",
        "ts" | "tsx" => "text/plain; charset=utf-8",
        "jsx" => "text/plain; charset=utf-8",
        // Default
        _ => "application/octet-stream",
    }
}
