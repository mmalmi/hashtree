/**
 * Gitignore parsing and matching utilities
 * Supports standard .gitignore patterns for filtering directory uploads
 */

export interface GitignorePattern {
  pattern: string;
  regex: RegExp;
  negation: boolean;
  directoryOnly: boolean;
}

/**
 * Parse a .gitignore file content into patterns
 */
export function parseGitignore(content: string): GitignorePattern[] {
  const patterns: GitignorePattern[] = [];

  for (const line of content.split('\n')) {
    const trimmed = line.trim();

    // Skip empty lines and comments
    if (!trimmed || trimmed.startsWith('#')) continue;

    let pattern = trimmed;
    let negation = false;
    let directoryOnly = false;

    // Handle negation
    if (pattern.startsWith('!')) {
      negation = true;
      pattern = pattern.slice(1);
    }

    // Handle directory-only patterns (ending with /)
    if (pattern.endsWith('/')) {
      directoryOnly = true;
      pattern = pattern.slice(0, -1);
    }

    // Convert gitignore pattern to regex
    const regex = patternToRegex(pattern);

    patterns.push({
      pattern: trimmed,
      regex,
      negation,
      directoryOnly,
    });
  }

  return patterns;
}

/**
 * Convert a gitignore pattern to a RegExp
 * Supports: *, **, ?, [abc], leading /, and more
 */
function patternToRegex(pattern: string): RegExp {
  let regexStr = '';
  let i = 0;

  // If pattern starts with /, it's anchored to root
  const anchored = pattern.startsWith('/');
  if (anchored) {
    pattern = pattern.slice(1);
    regexStr = '^';
  } else {
    // Match at any level
    regexStr = '(^|/)';
  }

  while (i < pattern.length) {
    const char = pattern[i];

    if (char === '*') {
      if (pattern[i + 1] === '*') {
        // ** matches any path segments
        if (pattern[i + 2] === '/') {
          // **/ matches zero or more directories
          regexStr += '(.*/)?';
          i += 3;
        } else if (i + 2 === pattern.length) {
          // trailing ** matches everything
          regexStr += '.*';
          i += 2;
        } else {
          // ** in middle
          regexStr += '.*';
          i += 2;
        }
      } else {
        // * matches anything except /
        regexStr += '[^/]*';
        i++;
      }
    } else if (char === '?') {
      // ? matches any single character except /
      regexStr += '[^/]';
      i++;
    } else if (char === '[') {
      // Character class - find closing bracket
      const closeIdx = pattern.indexOf(']', i + 1);
      if (closeIdx !== -1) {
        regexStr += pattern.slice(i, closeIdx + 1);
        i = closeIdx + 1;
      } else {
        regexStr += '\\[';
        i++;
      }
    } else if (char === '/') {
      regexStr += '/';
      i++;
    } else {
      // Escape special regex characters
      regexStr += char.replace(/[.+^${}()|\\]/g, '\\$&');
      i++;
    }
  }

  // Pattern can match the full path or just the end
  regexStr += '(/.*)?$';

  return new RegExp(regexStr);
}

/**
 * Check if a path should be ignored based on gitignore patterns
 * @param path - relative path from directory root (e.g., "src/foo.js" or "node_modules/pkg")
 * @param isDirectory - whether the path is a directory
 * @param patterns - parsed gitignore patterns
 */
export function isIgnored(
  path: string,
  isDirectory: boolean,
  patterns: GitignorePattern[]
): boolean {
  // Normalize path separators
  const normalizedPath = path.replace(/\\/g, '/');

  let ignored = false;

  for (const { regex, negation, directoryOnly } of patterns) {
    // Skip directory-only patterns for files
    if (directoryOnly && !isDirectory) continue;

    if (regex.test(normalizedPath)) {
      ignored = !negation;
    }
  }

  return ignored;
}

/**
 * Filter files based on gitignore patterns
 * Also checks parent directories - if a directory is ignored, all its contents are ignored
 */
export function filterByGitignore<T extends { relativePath: string }>(
  files: T[],
  patterns: GitignorePattern[]
): { included: T[]; excluded: T[] } {
  const included: T[] = [];
  const excluded: T[] = [];

  // Cache for directory ignore status
  const dirIgnoreCache = new Map<string, boolean>();

  for (const file of files) {
    const path = file.relativePath.replace(/\\/g, '/');

    // Check if any parent directory is ignored
    const parts = path.split('/');
    let parentIgnored = false;

    for (let i = 1; i < parts.length; i++) {
      const parentPath = parts.slice(0, i).join('/');

      if (!dirIgnoreCache.has(parentPath)) {
        dirIgnoreCache.set(parentPath, isIgnored(parentPath, true, patterns));
      }

      if (dirIgnoreCache.get(parentPath)) {
        parentIgnored = true;
        break;
      }
    }

    if (parentIgnored || isIgnored(path, false, patterns)) {
      excluded.push(file);
    } else {
      included.push(file);
    }
  }

  return { included, excluded };
}

/**
 * Common patterns to always ignore
 */
export const DEFAULT_IGNORE_PATTERNS = parseGitignore(`
# Common OS files
.DS_Store
Thumbs.db
`);
