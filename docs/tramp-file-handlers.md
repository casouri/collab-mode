# Tramp File Handler Analysis: Minimum Functions for a Remote Filesystem

This document analyzes Emacs Tramp's file handler implementation to determine the minimum functions required to implement a similar remote filesystem.

## Table of Contents

1. [How Tramp Works](#how-tramp-works)
2. [Complete List of Tramp Operations](#complete-list-of-tramp-operations)
3. [Minimum Functions for a Remote FS](#minimum-functions-for-a-remote-fs)
4. [Implementation Guide](#implementation-guide)
5. [Key Implementation Notes](#key-implementation-notes)
6. [Appendix A: Complete Function Reference](#appendix-a-complete-function-reference)
7. [Appendix B: Tramp Backend API](#appendix-b-tramp-backend-api)

---

## How Tramp Works

Tramp intercepts file operations by registering a handler in `file-name-handler-alist`:

```elisp
(add-to-list 'file-name-handler-alist
             (cons tramp-file-name-regexp #'tramp-file-name-handler))
```

The handler function receives `(operation &rest args)` and dispatches to the appropriate `tramp-handle-*` function. Unhandled operations fall through to default Emacs behavior.

**Key files:**
- `lisp/net/tramp.el` - Core handler and 56 `tramp-handle-*` functions (lines 4240-7158)
- `lisp/net/tramp-sh.el` - SSH/shell backend (42 backend-specific handlers)
- Handler registration: `tramp-register-file-name-handlers` (lines 2813-2856)

---

## Complete List of Tramp Operations

Tramp implements **56 file handler functions** organized into categories:

### File Attribute Operations (14)

| Function | Description |
|----------|-------------|
| `abbreviate-file-name` | Shorten path using directory abbreviations |
| `file-accessible-directory-p` | Check if directory is accessible |
| `file-directory-p` | Check if path is a directory |
| `file-equal-p` | Check if two paths refer to same file |
| `file-exists-p` | Check if file exists |
| `file-in-directory-p` | Check if file is inside directory |
| `file-modes` | Get file permission bits |
| `file-newer-than-file-p` | Compare modification times |
| `file-readable-p` | Check read permission |
| `file-regular-p` | Check if regular file (not dir/symlink) |
| `file-remote-p` | Return remote identification |
| `file-writable-p` | Check write permission |
| `file-user-uid` | Get file owner UID |
| `file-group-gid` | Get file group GID |

### File Name Operations (8)

| Function | Description |
|----------|-------------|
| `directory-file-name` | Remove trailing slash |
| `expand-file-name` | Make path absolute |
| `file-name-as-directory` | Add trailing slash |
| `file-name-case-insensitive-p` | Check case sensitivity |
| `file-name-completion` | Complete file name |
| `file-name-directory` | Extract directory part |
| `file-name-nondirectory` | Extract filename part |
| `substitute-in-file-name` | Expand $VAR in path |

### Symbolic Link Operations (3)

| Function | Description |
|----------|-------------|
| `file-symlink-p` | Check if symlink, return target |
| `file-truename` | Resolve all symlinks |
| `make-symbolic-link` | Create symlink |

### File Content Operations (3)

| Function | Description |
|----------|-------------|
| `file-local-copy` | Download to local temp file |
| `insert-file-contents` | Read file into buffer |
| `write-region` | Write buffer region to file |

### Directory Operations (4)

| Function | Description |
|----------|-------------|
| `directory-files` | List directory contents |
| `directory-files-and-attributes` | List with file attributes |
| `dired-uncache` | Clear directory cache |
| `insert-directory` | Insert ls-style listing |

### File Modification Operations (6)

| Function | Description |
|----------|-------------|
| `copy-file` | Copy a file |
| `copy-directory` | Copy directory recursively |
| `delete-file` | Delete a file |
| `delete-directory` | Delete a directory |
| `make-directory` | Create a directory |
| `rename-file` | Rename/move a file |

### File Locking (4)

| Function | Description |
|----------|-------------|
| `file-locked-p` | Check if file is locked |
| `lock-file` | Lock a file |
| `unlock-file` | Unlock a file |
| `make-lock-file-name` | Generate lock file path |

### Metadata Operations (7)

| Function | Description |
|----------|-------------|
| `access-file` | Check file accessibility |
| `add-name-to-file` | Create hard link |
| `file-attributes` | Get all file metadata |
| `set-file-modes` | Set permission bits |
| `set-file-times` | Set modification time |
| `set-visited-file-modtime` | Update buffer's modtime |
| `verify-visited-file-modtime` | Check if file changed |

### Process Operations (6)

| Function | Description |
|----------|-------------|
| `make-process` | Create async process |
| `process-file` | Run synchronous command |
| `shell-command` | Execute shell command |
| `start-file-process` | Start async process |
| `list-system-processes` | List remote processes |
| `process-attributes` | Get process info |

### Other Operations (5)

| Function | Description |
|----------|-------------|
| `load` | Load Lisp file |
| `temporary-file-directory` | Get temp directory |
| `make-nearby-temp-file` | Create temp file near path |
| `find-backup-file-name` | Generate backup name |
| `make-auto-save-file-name` | Generate autosave name |

---

## Minimum Functions for a Remote FS

### Tier 1: Critical (8 functions)

**Without these, nothing works.** These are the absolute minimum for opening and saving files:

| Function | Purpose | Why Essential |
|----------|---------|---------------|
| `file-exists-p` | Check if file exists | Required before any file operation |
| `file-directory-p` | Distinguish files from directories | Navigation and path handling |
| `file-attributes` | Return file metadata | Size, mtime, permissions for display |
| `expand-file-name` | Resolve relative paths | All file operations need absolute paths |
| `insert-file-contents` | Read file into buffer | Core of `find-file` |
| `write-region` | Write buffer to file | Core of `save-buffer` |
| `directory-files` | List directory contents | Completion, dired, navigation |
| `file-name-directory` | Extract directory part | Path manipulation |
| `file-name-nondirectory` | Extract filename part | Path manipulation |

### Tier 2: Basic Usability (+6 functions = 14 total)

**For practical daily use** with file management:

| Function | Purpose |
|----------|---------|
| `file-readable-p` | Check read permission before opening |
| `file-writable-p` | Check write permission before saving |
| `delete-file` | Remove files |
| `make-directory` | Create directories |
| `rename-file` | Move/rename files |
| `copy-file` | Copy files |

### Tier 3: Enhanced Experience (+5 functions = 19 total)

**For a polished experience:**

| Function | Purpose |
|----------|---------|
| `file-truename` | Resolve symlinks properly |
| `file-symlink-p` | Detect and handle symlinks |
| `file-local-copy` | Enable external tools (grep, compile) |
| `directory-files-and-attributes` | Faster dired listings |
| `set-file-modes` | chmod support |

### Tier 4: Full Featured (+6 functions = 25 total)

**For complete filesystem operations:**

| Function | Purpose |
|----------|---------|
| `copy-directory` | Recursive directory copy |
| `delete-directory` | Recursive directory delete |
| `make-symbolic-link` | Create symlinks |
| `file-newer-than-file-p` | Compilation dependency checks |
| `set-file-times` | Touch files |
| `add-name-to-file` | Hard links |

---

## Implementation Guide

### Basic Handler Structure

```elisp
(defvar my-remote-file-name-regexp "^/myremote:"
  "Regexp matching my remote file names.")

(defvar my-remote-operations-alist
  '((file-exists-p            . my-handle-file-exists-p)
    (file-directory-p         . my-handle-file-directory-p)
    (file-attributes          . my-handle-file-attributes)
    (expand-file-name         . my-handle-expand-file-name)
    (insert-file-contents     . my-handle-insert-file-contents)
    (write-region             . my-handle-write-region)
    (directory-files          . my-handle-directory-files)
    (file-name-directory      . my-handle-file-name-directory)
    (file-name-nondirectory   . my-handle-file-name-nondirectory))
  "Alist of operations handled by my remote filesystem.")

(defun my-remote-file-handler (operation &rest args)
  "Handle file OPERATION for my remote filesystem.
Falls through to default handler for unimplemented operations."
  (let ((handler (assoc operation my-remote-operations-alist)))
    (if handler
        (apply (cdr handler) args)
      ;; Fall through to default Emacs behavior
      (let ((inhibit-file-name-handlers
             (cons 'my-remote-file-handler inhibit-file-name-handlers))
            (inhibit-file-name-operation operation))
        (apply operation args)))))

;; Register the handler
(add-to-list 'file-name-handler-alist
             (cons my-remote-file-name-regexp #'my-remote-file-handler))
```

### Function Signatures

#### `file-exists-p`

```elisp
(defun my-handle-file-exists-p (filename)
  "Return t if FILENAME exists on the remote filesystem."
  (let ((remote-path (my-extract-remote-path filename)))
    ;; Your implementation: check if file exists
    ...))
```

#### `file-directory-p`

```elisp
(defun my-handle-file-directory-p (filename)
  "Return t if FILENAME is a directory."
  (let ((remote-path (my-extract-remote-path filename)))
    ;; Your implementation: check if directory
    ...))
```

#### `file-attributes`

```elisp
(defun my-handle-file-attributes (filename &optional id-format)
  "Return attributes of FILENAME as a list.
ID-FORMAT is 'string or 'integer for uid/gid format.

Returns a 12-element list:
  0. Type: t (dir), string (symlink target), or nil (regular file)
  1. Number of hard links
  2. UID (as string or integer per ID-FORMAT)
  3. GID (as string or integer per ID-FORMAT)
  4. Access time (current-time format or nil)
  5. Modification time (current-time format)
  6. Status change time (current-time format or nil)
  7. Size in bytes
  8. Mode string (e.g., \"-rw-r--r--\")
  9. Unused (nil for GID change flag)
  10. Inode number
  11. Device number"
  (let ((remote-path (my-extract-remote-path filename)))
    ;; Your implementation
    (list type links uid gid atime mtime ctime size mode nil inode device)))
```

#### `expand-file-name`

```elisp
(defun my-handle-expand-file-name (name &optional default-directory)
  "Make NAME absolute, defaulting to DEFAULT-DIRECTORY.
Must preserve the remote file prefix."
  (let ((dir (or default-directory default-directory)))
    ;; Preserve /myremote: prefix while normalizing the path
    ...))
```

#### `insert-file-contents`

```elisp
(defun my-handle-insert-file-contents (filename &optional visit beg end replace)
  "Insert contents of remote FILENAME into current buffer.
VISIT: if non-nil, set buffer-file-name and clear modification flag.
BEG/END: byte range to read (nil means entire file).
REPLACE: if non-nil, replace buffer contents.

Returns: (ABSOLUTE-FILENAME CHARS-INSERTED)"
  (let ((remote-path (my-extract-remote-path filename)))
    ;; 1. Fetch file content from remote
    ;; 2. Insert into buffer (respecting beg/end/replace)
    ;; 3. If visit, set buffer-file-name, clear modified flag
    ;; 4. Return (filename chars-inserted)
    ...
    (list (expand-file-name filename) chars-inserted)))
```

#### `write-region`

```elisp
(defun my-handle-write-region (start end filename &optional append visit lockname mustbenew)
  "Write region from START to END to remote FILENAME.
APPEND: if non-nil, append to file.
VISIT: if non-nil or string, set buffer-file-name.
LOCKNAME: file to use for locking.
MUSTBENEW: if non-nil, error if file exists."
  (let ((remote-path (my-extract-remote-path filename))
        (content (buffer-substring-no-properties start end)))
    ;; 1. Check mustbenew constraint
    ;; 2. Upload content to remote (append if requested)
    ;; 3. If visit, update buffer state
    ...))
```

#### `directory-files`

```elisp
(defun my-handle-directory-files (directory &optional full match nosort count)
  "Return list of files in remote DIRECTORY.
FULL: if non-nil, return absolute paths.
MATCH: regexp to filter filenames.
NOSORT: if non-nil, don't sort results.
COUNT: maximum number of files to return."
  (let ((remote-path (my-extract-remote-path directory)))
    ;; 1. List directory on remote
    ;; 2. Filter by match if provided
    ;; 3. Prepend directory if full
    ;; 4. Sort unless nosort
    ;; 5. Limit to count if provided
    ...))
```

---

## Key Implementation Notes

### 1. `file-attributes` Return Format

The return value is a **12-element list** that many Emacs functions depend on:

```elisp
;; Example for a regular file
'(nil                           ; 0: type (nil = regular file)
  1                             ; 1: link count
  1000                          ; 2: uid
  1000                          ; 3: gid
  (25483 54288 0 0)            ; 4: access time
  (25483 54288 0 0)            ; 5: modification time
  (25483 54288 0 0)            ; 6: status change time
  4096                          ; 7: size in bytes
  "-rw-r--r--"                  ; 8: mode string
  nil                           ; 9: unused
  12345678                      ; 10: inode
  2049)                         ; 11: device

;; For a directory, element 0 is t
;; For a symlink, element 0 is the link target string
```

### 2. Time Format

Emacs uses `(HIGH LOW MICROSEC PICOSEC)` format for times. Use `current-time` to get current time, or construct from Unix timestamp:

```elisp
(defun my-unix-to-emacs-time (unix-time)
  "Convert Unix timestamp to Emacs time format."
  (let ((high (ash unix-time -16))
        (low (logand unix-time #xFFFF)))
    (list high low 0 0)))
```

### 3. Fallback Mechanism

**Critical:** Always include fallback for unhandled operations. This allows Emacs functions you haven't implemented to still work (potentially incorrectly, but not crash):

```elisp
(let ((inhibit-file-name-handlers
       (cons 'my-remote-file-handler inhibit-file-name-handlers))
      (inhibit-file-name-operation operation))
  (apply operation args))
```

### 4. Path Extraction

Create a helper to extract the remote portion of paths:

```elisp
(defun my-extract-remote-path (filename)
  "Extract the remote path from FILENAME, removing the prefix."
  (if (string-match "^/myremote:\\(.*\\)" filename)
      (match-string 1 filename)
    filename))
```

### 5. `expand-file-name` Must Preserve Prefix

When expanding `/myremote:~/foo`, the result must still start with `/myremote:`:

```elisp
;; Wrong: "/home/user/foo"
;; Right: "/myremote:/home/user/foo"
```

### 6. `insert-file-contents` VISIT Flag

When `visit` is non-nil:
- Set `buffer-file-name` to the filename
- Set `buffer-file-truename`
- Clear the buffer's modified flag
- Update `buffer-file-modtime`

### 7. Error Handling

Use Emacs file error signals:

```elisp
(signal 'file-error (list "Opening file" "No such file" filename))
(signal 'file-already-exists (list "File exists" filename))
(signal 'permission-denied (list "Permission denied" filename))
```

---

## Summary Table

| Level | Functions | Capability |
|-------|-----------|------------|
| **Minimum Viable** | 8 | Open, edit, save files |
| **Basic Usability** | 14 | + File management |
| **Enhanced** | 19 | + Symlinks, external tools |
| **Full Featured** | 25 | + Recursive ops, links |
| **Tramp Complete** | 56 | + Processes, notifications |

For a proof-of-concept, implement the 8 Tier 1 functions. For daily use, implement 14-19 functions.

---

## References

- `lisp/net/tramp.el` - Main tramp implementation
- `lisp/net/tramp-sh.el` - SSH backend example
- Emacs Info: `(elisp) Magic File Names`
- [Tramp Manual](https://www.gnu.org/software/tramp/)

---

# Appendix A: Complete Function Reference

This appendix provides detailed descriptions of all 56 file handler functions that Tramp implements, organized by category.

---

## A.1 File Existence & Attributes (14 functions)

| Function | What It Does |
|----------|--------------|
| `file-exists-p` | Returns `t` if file/directory exists on remote. This is the most fundamental check - called before almost any file operation. |
| `file-directory-p` | Returns `t` if path is a directory. Used to distinguish files from directories for navigation, completion, and path handling. |
| `file-regular-p` | Returns `t` if it's a regular file (not directory, symlink, or special device file). Used when you need to ensure something is an actual file. |
| `file-readable-p` | Returns `t` if you have read permission. Called before opening files to provide meaningful error messages. |
| `file-writable-p` | Returns `t` if you have write permission. Called before saving to check if write will succeed. |
| `file-accessible-directory-p` | Returns `t` if directory exists AND is readable. Combines `file-directory-p` and `file-readable-p` for efficiency. |
| `file-modes` | Returns permission bits as integer (e.g., 493 for `rwxr-xr-x`, which is octal 0755). Used by chmod and permission displays. |
| `file-attributes` | Returns comprehensive 12-element list: type, link count, uid, gid, access time, modification time, status change time, size, mode string, unused, inode, device. The most important metadata function. |
| `file-newer-than-file-p` | Compares modification times of two files. Returns `t` if first file is newer. Used by compilation systems to check dependencies. |
| `file-equal-p` | Checks if two paths refer to the same file after resolving symlinks and canonicalizing paths. Used to detect circular references. |
| `file-in-directory-p` | Checks if a file is located inside a directory (accounting for symlinks). Used for security checks and path validation. |
| `file-remote-p` | Returns the remote identification string (e.g., `/ssh:host:`) or nil for local files. Essential for remote-aware code. |
| `file-user-uid` | Returns UID of file owner as integer. Used for ownership checks and display. |
| `file-group-gid` | Returns GID of file group as integer. Used for group permission checks. |

---

## A.2 Path Manipulation (8 functions)

| Function | What It Does |
|----------|--------------|
| `expand-file-name` | Converts relative path to absolute. Expands `~` to home directory, resolves `.` and `..`. Example: `~/foo` → `/home/user/foo`. Must preserve remote prefix! |
| `file-name-directory` | Extracts directory portion of path. Example: `/a/b/c.txt` → `/a/b/`. Returns nil for filename without directory. |
| `file-name-nondirectory` | Extracts filename portion of path. Example: `/a/b/c.txt` → `c.txt`. Returns empty string for paths ending in `/`. |
| `file-name-as-directory` | Ensures path ends with `/`. Example: `/a/b` → `/a/b/`. Used when you need a directory path for concatenation. |
| `directory-file-name` | Removes trailing `/` from directory path. Example: `/a/b/` → `/a/b`. Inverse of `file-name-as-directory`. |
| `abbreviate-file-name` | Shortens path using `~` for home and configured abbreviations. Example: `/home/user/foo` → `~/foo`. Used for display. |
| `substitute-in-file-name` | Expands `$VAR` and `${VAR}` environment variable references in path. Example: `$HOME/foo` → `/home/user/foo`. |
| `file-name-case-insensitive-p` | Returns `t` if the filesystem at path is case-insensitive (like macOS HFS+ or Windows NTFS). Affects completion and comparison. |

---

## A.3 Reading & Writing Files (3 functions)

| Function | What It Does |
|----------|--------------|
| `insert-file-contents` | Reads remote file content into current buffer. Arguments: `(filename &optional visit beg end replace)`. The `visit` flag sets `buffer-file-name` and clears modification flag. `beg`/`end` allow partial reads. Returns `(absolute-filename chars-inserted)`. This is the core of `find-file`. |
| `write-region` | Writes buffer region to remote file. Arguments: `(start end filename &optional append visit lockname mustbenew)`. Handles appending, visiting (updating buffer state), locking, and checking for existing files. This is the core of `save-buffer`. |
| `file-local-copy` | Downloads remote file to a local temporary file. Returns the local temp path. Essential for external tools (grep, compilers, diff) that can't access remote files directly. The temp file should be deleted after use. |

---

## A.4 Directory Operations (4 functions)

| Function | What It Does |
|----------|--------------|
| `directory-files` | Lists directory contents. Arguments: `(directory &optional full match nosort count)`. Returns list of filenames. `full` prepends directory path, `match` filters by regexp, `nosort` skips alphabetical sorting, `count` limits results. Always includes `.` and `..`. |
| `directory-files-and-attributes` | Like `directory-files` but returns `(filename . attributes)` pairs where attributes is the `file-attributes` result. Much faster for dired than calling `file-attributes` on each file separately. |
| `insert-directory` | Inserts `ls`-style directory listing into buffer. Arguments: `(file switches &optional wildcard full-directory-p)`. Used by dired-mode. Must handle various `ls` switches like `-l`, `-a`, `-h`. |
| `dired-uncache` | Clears any cached directory listing for the given path. Called when directory contents may have changed. |

---

## A.5 File Modification Operations (6 functions)

| Function | What It Does |
|----------|--------------|
| `copy-file` | Copies file to new location. Arguments: `(file newname &optional ok-if-already-exists keep-time preserve-uid-gid preserve-permissions)`. Handles overwrite confirmation, timestamp preservation, and permission copying. |
| `rename-file` | Moves/renames file. Arguments: `(file newname &optional ok-if-already-exists)`. Works across directories. For remote files, may need to copy then delete. |
| `delete-file` | Deletes a file. Arguments: `(filename &optional trash)`. The `trash` argument sends to trash instead of permanent delete if supported. |
| `make-directory` | Creates directory. Arguments: `(dir &optional parents)`. The `parents` flag creates intermediate directories (like `mkdir -p`). |
| `delete-directory` | Deletes directory. Arguments: `(directory &optional recursive trash)`. Without `recursive`, fails if directory not empty. |
| `copy-directory` | Recursively copies directory tree. Arguments: `(directory newname &optional keep-time parents copy-contents)`. Complex operation that must handle subdirectories, symlinks, and permissions. |

---

## A.6 Symbolic Link Operations (3 functions)

| Function | What It Does |
|----------|--------------|
| `file-symlink-p` | If file is a symlink, returns the link target as a string. Returns nil for non-symlinks. Does NOT resolve the target - just reads what the link points to. |
| `file-truename` | Resolves ALL symlinks in path to get canonical absolute path. Follows chains of symlinks. Detects circular symlinks. Essential for comparing if two paths refer to same file. |
| `make-symbolic-link` | Creates a symbolic link. Arguments: `(target linkname &optional ok-if-already-exists)`. Creates `linkname` pointing to `target`. |

---

## A.7 File Locking (4 functions)

| Function | What It Does |
|----------|--------------|
| `file-locked-p` | Checks if file is locked. Returns nil (not locked), `t` (locked by current user), or username string (locked by other user). |
| `lock-file` | Creates a lock for the file. Lock files typically named `.#filename` containing user@host.pid:timestamp. Prevents concurrent editing. |
| `unlock-file` | Removes the lock file. Called when buffer is saved or killed. |
| `make-lock-file-name` | Generates the lock file path for a given filename. Usually `.#` prefix in same directory. |

---

## A.8 Buffer/Visit State Management (4 functions)

| Function | What It Does |
|----------|--------------|
| `set-visited-file-modtime` | Updates buffer's recorded modification time. Arguments: `(&optional time)`. Called after saving to record when file was last known to match buffer. Used by `verify-visited-file-modtime`. |
| `verify-visited-file-modtime` | Checks if file on disk has changed since buffer was read/saved. Returns `t` if unchanged, nil if modified externally. Triggers "file changed on disk" warnings. |
| `find-backup-file-name` | Generates backup filename for a file. Handles numbered backups, backup directory configuration. Example: `foo.txt` → `foo.txt~` or `foo.txt.~1~`. |
| `make-auto-save-file-name` | Generates auto-save filename. Usually `#filename#` format. Must be unique and in appropriate directory for remote files. |

---

## A.9 Process Execution (6 functions)

| Function | What It Does |
|----------|--------------|
| `process-file` | Runs command synchronously on remote host. Arguments: `(program &optional infile buffer display &rest args)`. Like `call-process` but for remote files. Returns exit code. Output goes to buffer. |
| `start-file-process` | Starts asynchronous process on remote host. Arguments: `(name buffer program &rest args)`. Returns process object. Output streams to buffer asynchronously. |
| `make-process` | Modern async process creation with keyword arguments. More flexible than `start-file-process`. Supports `:connection-type`, `:filter`, `:sentinel`, etc. |
| `shell-command` | Executes shell command on remote host. Arguments: `(command &optional output-buffer error-buffer)`. Higher-level than `process-file`. |
| `list-system-processes` | Returns list of PIDs running on remote system. Used by `proced` and process management tools. |
| `process-attributes` | Returns detailed info about a remote process: pid, user, command, CPU%, memory, etc. Used by `proced` for process listing. |

---

## A.10 Miscellaneous Operations (6 functions)

| Function | What It Does |
|----------|--------------|
| `load` | Loads and evaluates Lisp file from remote location. Arguments: `(file &optional noerror nomessage nosuffix must-suffix)`. Downloads and executes elisp code. |
| `access-file` | Checks file accessibility, signals error if not accessible. Arguments: `(filename string)`. The `string` is used in the error message. Unlike `file-readable-p`, this throws rather than returning nil. |
| `add-name-to-file` | Creates hard link. Arguments: `(file newname &optional ok-if-already-exists)`. Creates additional name pointing to same inode. Only works within same filesystem. |
| `temporary-file-directory` | Returns appropriate temp directory for remote host. May be `/tmp` on remote or a local directory depending on implementation. |
| `make-nearby-temp-file` | Creates temporary file "near" given path. Arguments: `(prefix &optional dir-flag suffix text)`. For remote files, creates temp on same remote host. |
| `file-name-completion` | Completes partial filename against directory contents. Arguments: `(file directory &optional predicate)`. Returns completed name, `t` if unique match, or nil if no match. Core of file completion. |

---

## A.11 Not Fully Implemented Functions

| Function | Status | Notes |
|----------|--------|-------|
| `file-notify-add-watch` | Explicitly throws "not supported" error | File system notifications not available for remote files in most backends |
| `file-notify-rm-watch` | Stub | No-op since add-watch doesn't work |
| `file-notify-valid-p` | Stub | Always returns nil |
| `file-selinux-context` | Returns empty context | Returns `(nil nil nil nil)` - SELinux not supported remotely |
| `memory-info` | Backend-specific | Only implemented for some backends |

---

## A.12 Handler Dispatch Summary

When Emacs calls any file operation on a remote path, the flow is:

```
1. User calls (find-file "/ssh:host:/path/file.txt")
2. find-file calls (insert-file-contents "/ssh:host:/path/file.txt" t)
3. Emacs checks file-name-handler-alist
4. Matches tramp-file-name-regexp → calls tramp-file-name-handler
5. tramp-file-name-handler looks up 'insert-file-contents in its alist
6. Dispatches to tramp-handle-insert-file-contents (or backend-specific handler)
7. Handler fetches content via SSH, inserts into buffer
8. Returns (absolute-filename bytes-read)
```

This transparent interception is what makes Tramp "magic" - remote files work with almost all Emacs commands without modification.

---

# Appendix B: Tramp Backend API

This appendix describes how to implement a custom Tramp backend, which provides a **much smaller API surface** than implementing a standalone file handler.

---

## B.1 Two Approaches Compared

| Approach | Functions to Implement | Infrastructure |
|----------|------------------------|----------------|
| Standalone file handler | 56 | None - you build everything |
| Tramp backend | 10-15 | Full - caching, connections, errors |

**Recommendation:** Use Tramp's backend API. You get caching, connection management, error handling, multi-hop support, and the familiar `/method:host:/path` syntax.

---

## B.2 Minimal Backend Implementation

A complete Tramp backend requires 7 components:

### Component 1: Method Name Constant

```elisp
(defconst tramp-mybackend-method "mymethod"
  "Method name for my custom backend.")
```

### Component 2: Method Registration

```elisp
(tramp--with-startup
 (add-to-list 'tramp-methods
              `(,tramp-mybackend-method
                ;; Optional method parameters
                (tramp-default-port 8080)
                (tramp-tmpdir "/tmp"))))
```

### Component 3: Detection Function

```elisp
;;;###tramp-autoload
(defsubst tramp-mybackend-file-name-p (vec-or-filename)
  "Check if VEC-OR-FILENAME is handled by my backend."
  (when-let ((vec (tramp-ensure-dissected-file-name vec-or-filename)))
    (string= (tramp-file-name-method vec) tramp-mybackend-method)))
```

### Component 4: Handler Alist

```elisp
(defconst tramp-mybackend-file-name-handler-alist
  '(;; Custom implementations (you write these)
    (file-exists-p . tramp-mybackend-handle-file-exists-p)
    (file-attributes . tramp-mybackend-handle-file-attributes)
    (file-readable-p . tramp-mybackend-handle-file-readable-p)
    (file-writable-p . tramp-mybackend-handle-file-writable-p)
    (insert-file-contents . tramp-mybackend-handle-insert-file-contents)
    (write-region . tramp-mybackend-handle-write-region)
    (directory-files . tramp-mybackend-handle-directory-files)
    (copy-file . tramp-mybackend-handle-copy-file)
    (rename-file . tramp-mybackend-handle-rename-file)
    (delete-file . tramp-mybackend-handle-delete-file)
    (make-directory . tramp-mybackend-handle-make-directory)
    (delete-directory . tramp-mybackend-handle-delete-directory)

    ;; Delegate to generic Tramp handlers
    (abbreviate-file-name . tramp-handle-abbreviate-file-name)
    (access-file . tramp-handle-access-file)
    (directory-file-name . tramp-handle-directory-file-name)
    (expand-file-name . tramp-handle-expand-file-name)
    (file-accessible-directory-p . tramp-handle-file-accessible-directory-p)
    (file-directory-p . tramp-handle-file-directory-p)
    (file-equal-p . tramp-handle-file-equal-p)
    (file-in-directory-p . tramp-handle-file-in-directory-p)
    (file-modes . tramp-handle-file-modes)
    (file-name-as-directory . tramp-handle-file-name-as-directory)
    (file-name-directory . tramp-handle-file-name-directory)
    (file-name-nondirectory . tramp-handle-file-name-nondirectory)
    (file-regular-p . tramp-handle-file-regular-p)
    (file-remote-p . tramp-handle-file-remote-p)

    ;; Mark unsupported operations
    (exec-path . ignore)
    (file-acl . ignore)
    (make-process . ignore)
    (process-file . ignore)
    (shell-command . ignore)
    (start-file-process . ignore))
  "Alist of handler functions for my backend.")
```

### Component 5: Main Handler Function

```elisp
;;;###tramp-autoload
(defun tramp-mybackend-file-name-handler (operation &rest args)
  "Invoke my backend handler for OPERATION with ARGS.
Falls back to default handler for unimplemented operations."
  (if-let ((fn (assoc operation tramp-mybackend-file-name-handler-alist)))
      (save-match-data (apply (cdr fn) args))
    (tramp-run-real-handler operation args)))
```

### Component 6: Handler Registration

```elisp
;;;###tramp-autoload
(tramp--with-startup
 (tramp-register-foreign-file-name-handler
  #'tramp-mybackend-file-name-p #'tramp-mybackend-file-name-handler))
```

### Component 7: Implement Your Handlers

```elisp
(defun tramp-mybackend-handle-file-exists-p (filename)
  "Check if FILENAME exists on remote."
  (with-parsed-tramp-file-name filename nil
    ;; Variables available: v (struct), method, user, host, port, localname
    (with-tramp-file-property v localname "file-exists-p"
      ;; Your implementation - returns t or nil
      (my-backend-check-exists host port localname))))

(defun tramp-mybackend-handle-file-attributes (filename &optional id-format)
  "Return attributes of FILENAME."
  (with-parsed-tramp-file-name filename nil
    (with-tramp-file-property v localname (format "file-attributes-%s" id-format)
      ;; Return 12-element list or nil
      (my-backend-get-attributes host port localname id-format))))

(defun tramp-mybackend-handle-insert-file-contents
    (filename &optional visit beg end replace)
  "Insert contents of FILENAME into current buffer."
  (with-parsed-tramp-file-name filename nil
    (let ((content (my-backend-read-file host port localname beg end)))
      (when replace (erase-buffer))
      (insert content)
      (when visit
        (setq buffer-file-name filename)
        (set-buffer-modified-p nil))
      (list (expand-file-name filename) (length content)))))

(defun tramp-mybackend-handle-write-region
    (start end filename &optional append visit lockname mustbenew)
  "Write region to FILENAME."
  (with-parsed-tramp-file-name filename nil
    (when (and mustbenew (file-exists-p filename))
      (signal 'file-already-exists (list filename)))
    (let ((content (buffer-substring-no-properties start end)))
      (my-backend-write-file host port localname content append)
      (when visit
        (setq buffer-file-name filename)
        (set-buffer-modified-p nil)))))
```

---

## B.3 What Tramp Provides For Free

### Generic Handlers

These `tramp-handle-*` functions work for most backends without modification:

```
tramp-handle-abbreviate-file-name
tramp-handle-access-file
tramp-handle-directory-file-name
tramp-handle-directory-files          ; If you implement file-attributes
tramp-handle-expand-file-name
tramp-handle-file-accessible-directory-p
tramp-handle-file-directory-p         ; Uses file-attributes
tramp-handle-file-equal-p
tramp-handle-file-in-directory-p
tramp-handle-file-modes               ; Uses file-attributes
tramp-handle-file-name-as-directory
tramp-handle-file-name-directory
tramp-handle-file-name-nondirectory
tramp-handle-file-regular-p           ; Uses file-attributes
tramp-handle-file-remote-p
tramp-handle-find-backup-file-name
tramp-handle-insert-directory         ; Uses directory-files-and-attributes
tramp-handle-make-auto-save-file-name
tramp-handle-set-visited-file-modtime
tramp-handle-verify-visited-file-modtime
... and 30+ more
```

### Skeleton Macros

Pre-built operation patterns that handle common logic:

```elisp
tramp-skeleton-copy-directory      ; Handles recursive copying
tramp-skeleton-delete-directory    ; Handles recursive deletion
tramp-skeleton-delete-file         ; Handles trash and confirmation
tramp-skeleton-directory-files     ; Handles filtering and sorting
tramp-skeleton-file-exists-p       ; Handles caching
tramp-skeleton-file-local-copy     ; Handles temp file creation
tramp-skeleton-make-directory      ; Handles parent creation
tramp-skeleton-write-region        ; Handles visit and locking
```

### Utility Macros

```elisp
;; Parse filename into components
(with-parsed-tramp-file-name filename nil
  ;; Available: v, method, user, domain, host, port, localname, hop
  (message "Connecting to %s:%s" host port))

;; Cache file properties
(with-tramp-file-property v localname "property-name"
  ;; Body only evaluated if not cached
  (expensive-operation))

;; Show progress
(with-tramp-progress-reporter v 0 "Uploading file"
  (upload-file ...))

;; Signal error
(tramp-error v 'file-error "Cannot connect to %s" host)

;; Log message (respects tramp-verbose)
(tramp-message v 5 "Debug: reading %s" localname)
```

### Infrastructure

- **Connection caching** - reuse connections
- **Property caching** - avoid redundant remote calls
- **Password management** - auth-source integration
- **Timeout handling** - configurable timeouts
- **Error handling** - proper Emacs error signals
- **Debug logging** - controllable verbosity

---

## B.4 Non-Shell Backend Examples

### tramp-adb.el (Android Debug Bridge)

Uses `adb` commands, no remote shell:

```elisp
;; Read file via adb pull
(defun tramp-adb-handle-insert-file-contents (...)
  (let ((local-copy (tramp-adb-handle-file-local-copy filename)))
    (insert-file-contents local-copy visit beg end replace)
    (delete-file local-copy)))

;; Write file via adb push
(defun tramp-adb-handle-write-region (...)
  (let ((tmpfile (tramp-compat-make-temp-file filename)))
    (write-region start end tmpfile)
    (tramp-adb-command v (format "push %s %s" tmpfile localname))
    (delete-file tmpfile)))
```

**Implements:** ~18 functions
**Marks as `ignore`:** `file-acl`, `file-ownership-preserved-p`, `tramp-set-file-uid-gid`

### tramp-gvfs.el (GVFS/D-Bus)

Uses D-Bus to mount remote filesystem locally:

```elisp
;; Mount via D-Bus, then delegate to local filesystem
(defun tramp-gvfs-handle-file-attributes (filename &optional id-format)
  (with-parsed-tramp-file-name filename nil
    (tramp-gvfs-maybe-open-connection v)  ; Ensure mounted
    (let ((local-path (tramp-gvfs-local-file-name v)))
      (file-attributes local-path id-format))))
```

**Implements:** ~17 functions
**Marks as `ignore`:** `exec-path`, `make-process`, `process-file`, `shell-command`, `start-file-process`

### tramp-smb.el (Samba/SMB)

Uses `smbclient` program:

```elisp
;; List directory via smbclient
(defun tramp-smb-handle-directory-files (directory &optional full match nosort count)
  (let ((result (tramp-smb-get-file-entries directory)))
    ;; Parse smbclient output into file list
    ...))
```

**Implements:** ~30 functions
**Marks as `ignore`:** `exec-path`, `list-system-processes`, `memory-info`, `process-attributes`

---

## B.5 Minimum Functions to Implement

For a working non-shell backend:

| Function | Required? | Notes |
|----------|-----------|-------|
| `file-exists-p` | **Yes** | Core check |
| `file-attributes` | **Yes** | Many handlers derive from this |
| `insert-file-contents` | **Yes** | Read files |
| `write-region` | **Yes** | Write files |
| `directory-files` | **Yes** | List directories |
| `copy-file` | **Yes** | File operations |
| `rename-file` | **Yes** | File operations |
| `delete-file` | **Yes** | File operations |
| `make-directory` | **Yes** | Directory operations |
| `delete-directory` | **Yes** | Directory operations |
| `file-readable-p` | Recommended | Better error messages |
| `file-writable-p` | Recommended | Better error messages |

**Total: 10-12 functions** vs 56 for standalone implementation.

---

## B.6 Registration Priority

Backends are checked in order. The shell handler is registered LAST as a fallback:

```elisp
;; Your backend - specific method match, registered first
(tramp-register-foreign-file-name-handler
 #'tramp-mybackend-file-name-p    ; Returns t only for "mymethod"
 #'tramp-mybackend-file-name-handler)

;; ADB backend - matches "adb" method
;; GVFS backend - matches "sftp", "gdrive", etc.
;; SMB backend - matches "smb" method

;; Shell handler - uses #'identity (matches everything)
;; Registered with 'append' flag, so it's checked LAST
(tramp-register-foreign-file-name-handler
 #'identity #'tramp-sh-file-name-handler 'append)
```

---

## B.7 Example: HTTP/REST Backend Skeleton

```elisp
;;; tramp-http.el --- Tramp backend for HTTP/REST APIs

(defconst tramp-http-method "http")

(defun tramp-http-api-call (host port endpoint method &optional body)
  "Make HTTP API call to HOST:PORT/ENDPOINT with METHOD and BODY."
  (let ((url (format "http://%s:%s/api%s" host (or port 80) endpoint)))
    (with-current-buffer (url-retrieve-synchronously url)
      (goto-char (point-min))
      (re-search-forward "\n\n")
      (json-read))))

(defun tramp-http-handle-file-exists-p (filename)
  (with-parsed-tramp-file-name filename nil
    (with-tramp-file-property v localname "file-exists-p"
      (condition-case nil
          (progn (tramp-http-api-call host port localname "HEAD") t)
        (error nil)))))

(defun tramp-http-handle-file-attributes (filename &optional id-format)
  (with-parsed-tramp-file-name filename nil
    (with-tramp-file-property v localname "file-attributes"
      (let ((meta (tramp-http-api-call host port
                    (concat localname "?meta=true") "GET")))
        (list (if (alist-get 'is_dir meta) t nil)  ; type
              1                                      ; links
              (alist-get 'uid meta)                 ; uid
              (alist-get 'gid meta)                 ; gid
              nil                                    ; atime
              (alist-get 'mtime meta)               ; mtime
              nil                                    ; ctime
              (alist-get 'size meta)                ; size
              (alist-get 'mode meta)                ; mode
              nil nil nil)))))                       ; unused, inode, device

(defun tramp-http-handle-insert-file-contents
    (filename &optional visit beg end replace)
  (with-parsed-tramp-file-name filename nil
    (let ((content (tramp-http-api-call host port localname "GET")))
      (when replace (erase-buffer))
      (insert (if (and beg end)
                  (substring content beg end)
                content))
      (when visit
        (setq buffer-file-name filename)
        (set-buffer-modified-p nil))
      (list filename (length content)))))

;; ... implement remaining handlers ...

;;;###tramp-autoload
(tramp--with-startup
 (add-to-list 'tramp-methods `(,tramp-http-method))
 (tramp-register-foreign-file-name-handler
  #'tramp-http-file-name-p #'tramp-http-file-name-handler))

(provide 'tramp-http)
;;; tramp-http.el ends here
```

---

## B.8 Summary

| Aspect | Standalone Handler | Tramp Backend |
|--------|-------------------|---------------|
| Functions to write | 56 | 10-15 |
| Caching | Build yourself | Included |
| Connection pooling | Build yourself | Included |
| Password management | Build yourself | Included |
| Error handling | Build yourself | Included |
| User syntax | Custom | `/method:host:/path` |
| Multi-hop | Build yourself | Included |
| Works with dired | If you implement it | Yes |

**Use Tramp's backend API** unless you have a specific reason not to. The reduced implementation surface and free infrastructure make it the clear choice for most remote filesystem implementations.
