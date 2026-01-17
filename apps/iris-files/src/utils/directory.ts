/**
 * Directory reading utilities for browser File System Access API
 * Supports both webkitdirectory file inputs and drag-and-drop directories
 */

import {
  parseGitignore,
  filterByGitignore,
  DEFAULT_IGNORE_PATTERNS,
  type GitignorePattern,
} from './gitignore';

export interface FileWithPath {
  file: File;
  /** Relative path from the dropped/selected directory root */
  relativePath: string;
}

export interface DirectoryReadResult {
  files: FileWithPath[];
  /** If a .gitignore was found at the root of the directory */
  hasGitignore: boolean;
  /** Parsed gitignore patterns (if found) */
  gitignorePatterns: GitignorePattern[] | null;
  /** Root directory name */
  rootDirName: string | null;
}

/**
 * Read files from a FileList that was selected via input[webkitdirectory]
 * Files already have webkitRelativePath set by the browser
 */
export function readFilesFromWebkitDirectory(files: FileList): DirectoryReadResult {
  const result: FileWithPath[] = [];
  let gitignoreFile: File | null = null;
  let rootDirName: string | null = null;

  for (let i = 0; i < files.length; i++) {
    const file = files[i];
    if (!file) continue;

    // webkitRelativePath includes the root directory name, e.g., "mydir/subdir/file.txt"
    // We want to keep that structure
    const relativePath = (file as File & { webkitRelativePath?: string }).webkitRelativePath || file.name;

    // Extract root directory name from first file
    if (!rootDirName && relativePath.includes('/')) {
      rootDirName = relativePath.split('/')[0];
    }

    // Check for .gitignore at root level (e.g., "mydir/.gitignore")
    const pathParts = relativePath.split('/');
    if (pathParts.length === 2 && pathParts[1] === '.gitignore') {
      gitignoreFile = file;
    }

    result.push({ file, relativePath });
  }

  return {
    files: result,
    hasGitignore: gitignoreFile !== null,
    gitignorePatterns: null, // Will be populated after reading file content
    rootDirName,
  };
}

/**
 * Read and parse .gitignore content from a file
 */
export async function parseGitignoreFromFile(file: File): Promise<GitignorePattern[]> {
  const content = await file.text();
  return parseGitignore(content);
}

/**
 * Find .gitignore file in a list of files (at root level)
 */
export function findGitignoreFile(files: FileWithPath[], rootDirName: string | null): FileWithPath | null {
  // Look for .gitignore at the root of the uploaded directory
  // Path would be like "mydir/.gitignore" or just ".gitignore"
  return files.find(f => {
    const parts = f.relativePath.split('/');
    if (rootDirName) {
      // webkitdirectory: "rootDir/.gitignore"
      return parts.length === 2 && parts[0] === rootDirName && parts[1] === '.gitignore';
    } else {
      // drag-and-drop single dir: "dirName/.gitignore"
      return parts.length === 2 && parts[1] === '.gitignore';
    }
  }) || null;
}

/**
 * Apply gitignore filtering to files
 */
export function applyGitignoreFilter(
  files: FileWithPath[],
  patterns: GitignorePattern[],
  includeDefaults = true
): { included: FileWithPath[]; excluded: FileWithPath[] } {
  const allPatterns = includeDefaults ? [...DEFAULT_IGNORE_PATTERNS, ...patterns] : patterns;
  return filterByGitignore(files, allPatterns);
}

/**
 * Apply only default ignore patterns (.git, .DS_Store, etc.)
 */
export function applyDefaultIgnoreFilter(
  files: FileWithPath[]
): { included: FileWithPath[]; excluded: FileWithPath[] } {
  return filterByGitignore(files, DEFAULT_IGNORE_PATTERNS);
}

/**
 * Check if a DataTransfer contains directory items
 */
export function hasDirectoryItems(dataTransfer: DataTransfer): boolean {
  if (!dataTransfer.items) return false;

  for (let i = 0; i < dataTransfer.items.length; i++) {
    const item = dataTransfer.items[i];
    if (item?.kind === 'file') {
      const entry = item.webkitGetAsEntry?.();
      if (entry?.isDirectory) return true;
    }
  }

  return false;
}

/**
 * Read all files from a FileSystemEntry recursively
 */
async function readEntry(entry: FileSystemEntry, basePath: string): Promise<FileWithPath[]> {
  if (entry.isFile) {
    const fileEntry = entry as FileSystemFileEntry;
    const file = await new Promise<File>((resolve, reject) => {
      fileEntry.file(resolve, reject);
    });
    return [{ file, relativePath: basePath }];
  }

  if (entry.isDirectory) {
    const dirEntry = entry as FileSystemDirectoryEntry;
    const reader = dirEntry.createReader();
    const results: FileWithPath[] = [];

    // readEntries may not return all entries at once, so we need to call it repeatedly
    let entries: FileSystemEntry[] = [];
    do {
      const batch = await new Promise<FileSystemEntry[]>((resolve, reject) => {
        reader.readEntries(resolve, reject);
      });
      entries = batch;

      for (const childEntry of entries) {
        const childPath = basePath ? `${basePath}/${childEntry.name}` : childEntry.name;
        const childFiles = await readEntry(childEntry, childPath);
        results.push(...childFiles);
      }
    } while (entries.length > 0);

    return results;
  }

  return [];
}

/**
 * Read files from drag-and-drop DataTransfer, supporting directories
 * Uses webkitGetAsEntry() for directory access
 */
export async function readFilesFromDataTransfer(dataTransfer: DataTransfer): Promise<DirectoryReadResult> {
  const results: FileWithPath[] = [];
  let rootDirName: string | null = null;

  // Check if we have the directory-capable API
  if (dataTransfer.items) {
    for (let i = 0; i < dataTransfer.items.length; i++) {
      const item = dataTransfer.items[i];
      if (item?.kind !== 'file') continue;

      const entry = item.webkitGetAsEntry?.();
      if (entry) {
        // Track root directory name for single directory drops
        if (entry.isDirectory && !rootDirName) {
          rootDirName = entry.name;
        }
        // Use entry API for potentially directory support
        const files = await readEntry(entry, entry.name);
        results.push(...files);
      } else {
        // Fallback to regular file
        const file = item.getAsFile();
        if (file) {
          results.push({ file, relativePath: file.name });
        }
      }
    }
  } else if (dataTransfer.files) {
    // Fallback for browsers without items API
    for (let i = 0; i < dataTransfer.files.length; i++) {
      const file = dataTransfer.files[i];
      if (file) {
        results.push({ file, relativePath: file.name });
      }
    }
  }

  // Check for .gitignore at root
  const gitignoreFileEntry = findGitignoreFile(results, rootDirName);

  return {
    files: results,
    hasGitignore: gitignoreFileEntry !== null,
    gitignorePatterns: null,
    rootDirName,
  };
}

/**
 * Check if browser supports directory upload via webkitdirectory
 */
export function supportsDirectoryUpload(): boolean {
  // Check if the webkitdirectory attribute is supported
  const input = document.createElement('input');
  return 'webkitdirectory' in input;
}
