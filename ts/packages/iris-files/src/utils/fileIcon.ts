/**
 * Get icon class based on file extension
 */
export function getFileIcon(filename: string): string {
  const ext = filename.split('.').pop()?.toLowerCase() || '';
  switch (ext) {
    case 'jpg': case 'jpeg': case 'png': case 'gif': case 'webp': case 'svg': case 'ico': case 'bmp':
      return 'i-lucide-image';
    case 'mp4': case 'webm': case 'mkv': case 'avi': case 'mov':
      return 'i-lucide-video';
    case 'mp3': case 'wav': case 'ogg': case 'flac': case 'm4a':
      return 'i-lucide-music';
    case 'exe': case 'com': case 'bat':
      return 'i-lucide-terminal';
    case 'js': case 'ts': case 'jsx': case 'tsx': case 'py': case 'rb': case 'go': case 'rs': case 'c': case 'cpp': case 'h': case 'java': case 'php': case 'sh': case 'bash':
      return 'i-lucide-file-code';
    case 'json': case 'yaml': case 'yml': case 'toml': case 'xml': case 'ini': case 'env':
      return 'i-lucide-file-json';
    case 'pdf': case 'doc': case 'docx': case 'txt': case 'md': case 'markdown': case 'rst':
      return 'i-lucide-file-text';
    case 'xls': case 'xlsx': case 'csv':
      return 'i-lucide-file-spreadsheet';
    case 'ppt': case 'pptx':
      return 'i-lucide-file-presentation';
    case 'zip': case 'tar': case 'gz': case 'rar': case '7z':
      return 'i-lucide-file-archive';
    case 'html': case 'htm': case 'css': case 'scss': case 'sass': case 'less':
      return 'i-lucide-file-code';
    default:
      return 'i-lucide-file';
  }
}
