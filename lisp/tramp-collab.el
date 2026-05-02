;;; tramp-collab.el --- Tramp backend for collab-mode  -*- lexical-binding: t; -*-

;; Author: Yuan Fu <casouri@gmail.com>

;;; This file is NOT part of GNU Emacs

;;; Commentary:

;; A minimal Tramp backend that exposes collab-mode’s remote files as a
;; regular Emacs filesystem.  The path syntax is
;;
;;     /collab:HOST-ID:/PROJECT/FILE-PATH
;;
;; where HOST-ID is the collab host id, PROJECT is the project name and
;; FILE-PATH is the file path inside the project.  Both PROJECT and
;; FILE-PATH may be empty: an empty FILE-PATH refers to the project root
;; and an empty PROJECT refers to the host root (a list of projects).
;;
;; A remote filesystem is considered to exist only when collab’s
;; ConnectionState reports the host as “Connected”.  When the host is
;; absent or disconnected, all listing/lookup handlers return nil and
;; opening a file errors out.
;;
;; Reading uses OpenFile and, if the buffer isn’t already monitored,
;; immediately flips it into ‘collab-monitored-mode’ so subsequent
;; edits sync.  Saving
;; refuses to upload buffer text and instead asks collab to persist the
;; current remote document via SaveFile — which only makes sense if the
;; buffer is already in monitored mode, so non-monitored writes signal a
;; file-error rather than silently dropping edits.

;;; Code:

(require 'tramp)
(require 'tramp-cache)
(require 'jsonrpc)

;; Forward declarations — collab-mode loads this file at the end of its
;; own load, so all of these are bound by the time anything here runs.
;; We avoid (require 'collab-mode) to break the load cycle.
(defvar collab--jsonrpc-connection)
(defvar collab--my-host-id)
(defvar collab--file)
(defvar collab-monitored-mode)
(defvar collab-host-alist)
(declare-function collab--make-file-desc "collab-mode")
(declare-function collab--host-alist "collab-mode")
(declare-function collab--host-state "collab-mode")
(declare-function collab--connection-state-req "collab-mode")
(declare-function collab--list-files-req "collab-mode")
(declare-function collab--open-file-req "collab-mode")
(declare-function collab--save-file-req "collab-mode")
(declare-function collab--enable "collab-mode")
(declare-function collab--encode-filename "collab-mode")

(defconst tramp-collab-method "collab"
  "Tramp method name for the collab backend.")

;;;; Path parsing

(defun tramp-collab--split-localname (localname)
  "Split LOCALNAME into a list (PROJECT FILE).
LOCALNAME is the part after the host in a Tramp filename, e.g.
“/myproj/sub/foo.txt” → (\"myproj\" \"sub/foo.txt\").  Both halves
may be empty strings."
  (let* ((trimmed (if (string-prefix-p "/" localname)
                      (substring localname 1)
                    localname))
         (segments (split-string trimmed "/" t)))
    (cond
     ((null segments) (list "" ""))
     ((null (cdr segments)) (list (car segments) ""))
     (t (list (car segments) (mapconcat #'identity (cdr segments) "/"))))))

(defun tramp-collab--vec-file-desc (vec)
  "Return the collab FILE-DESC for VEC."
  (let ((host (tramp-file-name-host vec))
        (parts (tramp-collab--split-localname
                (tramp-file-name-localname vec))))
    (collab--make-file-desc host (nth 0 parts) (nth 1 parts))))

(defun tramp-collab--parent-vec (vec)
  "Return a vec for the parent directory of VEC, or nil at the host root."
  (let* ((localname (tramp-file-name-localname vec))
         (trimmed (string-trim-right localname "/")))
    (when (and (not (equal trimmed ""))
               (string-search "/" trimmed))
      (let ((parent (file-name-directory trimmed)))
        (tramp-dissect-file-name
         (tramp-make-tramp-file-name vec parent))))))

;;;; Connection guard

(defun tramp-collab--connected-p (host-id)
  "Return non-nil if HOST-ID is currently usable as a collab remote.
We never auto-spawn the collab process from a Tramp call: if the
JSONRPC connection is not already up, the remote is treated as
unavailable."
  (cond
   ((or (null collab--jsonrpc-connection)
        (not (jsonrpc-running-p collab--jsonrpc-connection)))
    nil)
   ((equal host-id collab--my-host-id) t)
   (t
    (condition-case nil
        (equal "Connected"
               (collab--host-state
                host-id
                (plist-get (collab--connection-state-req) :connections)))
      (error nil)))))

;;;; Listing

(defun tramp-collab--list-files (vec)
  "Return the listing for the directory at VEC.
For the host root this is the project list; for a project root or
subdirectory this is the file/dir listing.  Returns the raw
ListFiles/ListProjects entries (a vector of plists) or nil when
unavailable.  Cached per (host, localname)."
  (let ((host (tramp-file-name-host vec))
        (localname (tramp-file-name-localname vec)))
    (if (not (tramp-collab--connected-p host))
        nil
      (with-tramp-file-property vec localname "list-files"
        (condition-case nil
            (let* ((file-desc (tramp-collab--vec-file-desc vec))
                   (host-data (alist-get host (collab--host-alist)
                                         nil nil #'equal))
                   (signaling (or (nth 0 host-data) ""))
                   (credential (or (nth 1 host-data) ""))
                   ;; Empty project ⇒ host root ⇒ ListProjects.
                   (dir (if (equal "" (plist-get file-desc :project))
                            nil
                          file-desc))
                   (resp (collab--list-files-req
                          dir host signaling credential)))
              (plist-get resp :files))
          (error nil))))))

(defun tramp-collab--lookup-entry (vec)
  "Look up VEC’s basename in its parent directory listing.
Return the matching entry plist or nil."
  (let* ((localname (tramp-file-name-localname vec))
         (trimmed (string-trim-right localname "/"))
         (basename (file-name-nondirectory trimmed))
         (parent-vec (tramp-collab--parent-vec vec)))
    (when (and parent-vec (not (equal basename "")))
      (seq-find (lambda (e) (equal (plist-get e :filename) basename))
                (tramp-collab--list-files parent-vec)))))

;;;; Synthetic file-attributes

(defun tramp-collab--dir-attrs ()
  "Return synthetic 12-element attrs list for a directory."
  (list t 1 0 0 nil (current-time) nil 0 "drwxr-xr-x" nil 0 0))

(defun tramp-collab--file-attrs ()
  "Return synthetic 12-element attrs list for a regular file."
  (list nil 1 0 0 nil (current-time) nil 0 "-rw-r--r--" nil 0 0))

(defun tramp-collab--entry-attrs (entry)
  "Return attrs list matching ENTRY (an element of ListFiles)."
  (if (eq (plist-get entry :isDirectory) :json-false)
      (tramp-collab--file-attrs)
    (tramp-collab--dir-attrs)))

(defun tramp-collab--at-host-root-p (localname)
  "Return non-nil if LOCALNAME refers to the host root (“/” or “”)."
  (let ((trimmed (string-trim-right localname "/")))
    (or (equal trimmed "") (equal localname "/"))))

;;;; Handlers — listing & metadata

(defun tramp-collab-handle-file-exists-p (filename)
  "Return non-nil if FILENAME exists in collab."
  (with-parsed-tramp-file-name filename nil
    (cond
     ((not (tramp-collab--connected-p host)) nil)
     ((tramp-collab--at-host-root-p localname) t)
     (t (and (tramp-collab--lookup-entry v) t)))))

(defun tramp-collab-handle-file-readable-p (filename)
  "Like ‘file-readable-p’ for collab.  Mirrors ‘file-exists-p’."
  (tramp-collab-handle-file-exists-p filename))

(defun tramp-collab-handle-file-writable-p (filename)
  "Like ‘file-writable-p’ for collab.  Mirrors ‘file-exists-p’."
  (tramp-collab-handle-file-exists-p filename))

(defun tramp-collab-handle-file-attributes (filename &optional _id-format)
  "Return collab attributes for FILENAME, or nil if not present."
  (with-parsed-tramp-file-name filename nil
    (cond
     ((not (tramp-collab--connected-p host)) nil)
     ((tramp-collab--at-host-root-p localname)
      (tramp-collab--dir-attrs))
     (t
      (when-let* ((entry (tramp-collab--lookup-entry v)))
        (tramp-collab--entry-attrs entry))))))

(defun tramp-collab--filter-and-sort (names match nosort count)
  "Apply MATCH/NOSORT/COUNT to NAMES (a list of strings)."
  (let* ((filtered (if match
                       (seq-filter (lambda (n) (string-match-p match n)) names)
                     names))
         (sorted (if nosort filtered (sort filtered #'string<))))
    (if count (seq-take sorted count) sorted)))

(defun tramp-collab-handle-directory-files
    (directory &optional full match nosort count)
  "Like ‘directory-files’ for collab paths."
  (with-parsed-tramp-file-name (expand-file-name directory) nil
    (when (tramp-collab--connected-p host)
      (let* ((entries (tramp-collab--list-files v))
             (names (mapcar (lambda (e) (plist-get e :filename)) entries))
             (with-dots (append '("." "..") names))
             (result (tramp-collab--filter-and-sort
                      with-dots match nosort count))
             (dir (file-name-as-directory directory)))
        (if full (mapcar (lambda (n) (concat dir n)) result) result)))))

(defun tramp-collab-handle-file-name-all-completions (filename directory)
  "Like ‘file-name-all-completions’ for collab DIRECTORY."
  (tramp-skeleton-file-name-all-completions filename directory
    (with-parsed-tramp-file-name (expand-file-name directory) nil
      (when (tramp-collab--connected-p host)
        (let ((entries (tramp-collab--list-files v)))
          (all-completions
           filename
           (mapcar
            (lambda (e)
              (let ((name (plist-get e :filename))
                    (dir-p (not (eq (plist-get e :isDirectory)
                                    :json-false))))
                (if dir-p (file-name-as-directory name) name)))
            entries)))))))

(defun tramp-collab-handle-directory-files-and-attributes
    (directory &optional full match nosort _id-format count)
  "Like ‘directory-files-and-attributes’ for collab paths."
  (with-parsed-tramp-file-name (expand-file-name directory) nil
    (when (tramp-collab--connected-p host)
      (let* ((entries (tramp-collab--list-files v))
             (pairs (mapcar
                     (lambda (e)
                       (cons (plist-get e :filename)
                             (tramp-collab--entry-attrs e)))
                     entries))
             (all (append (list (cons "." (tramp-collab--dir-attrs))
                                (cons ".." (tramp-collab--dir-attrs)))
                          pairs))
             (filtered (if match
                           (seq-filter (lambda (p)
                                         (string-match-p match (car p)))
                                       all)
                         all))
             (sorted (if nosort filtered
                       (sort filtered (lambda (a b)
                                        (string< (car a) (car b))))))
             (limited (if count (seq-take sorted count) sorted))
             (dir (file-name-as-directory directory)))
        (if full
            (mapcar (lambda (p) (cons (concat dir (car p)) (cdr p))) limited)
          limited)))))

;;;; Handlers — file content

(defun tramp-collab-handle-insert-file-contents
    (filename &optional visit beg end replace)
  "Insert contents of collab FILENAME into the current buffer.
After inserting, if the buffer isn’t already collab-monitored,
flip it into ‘collab-monitored-mode’ using the OpenFile response."
  (barf-if-buffer-read-only)
  (let ((expanded (expand-file-name filename)))
    (with-parsed-tramp-file-name expanded nil
      (when (not (tramp-collab--connected-p host))
        (signal 'file-error
                (list "Collab host not connected" host expanded)))
      (let* ((file-desc (tramp-collab--vec-file-desc v))
             (resp (collab--open-file-req file-desc "create"))
             (site-id (plist-get resp :siteId))
             (content (or (plist-get resp :content) ""))
             (resp-fd (plist-get resp :file))
             (slice (cond ((and beg end) (substring content beg end))
                          (beg (substring content beg))
                          (end (substring content 0 end))
                          (t content))))
        (when replace
          (delete-region (point-min) (point-max)))
        (insert slice)
        (goto-char (point-min))
        (when visit
          (setq buffer-file-name expanded)
          (set-buffer-modified-p nil)
          (set-visited-file-modtime))
        (unless (bound-and-true-p collab-monitored-mode)
          (condition-case err
              (collab--enable resp-fd site-id
                              (collab--encode-filename resp-fd))
            (error
             (message "tramp-collab: failed to enable monitor: %S" err))))
        (list expanded (length slice))))))

(defun tramp-collab-handle-write-region
    (_start _end filename &optional _append visit _lockname _mustbenew)
  "Persist collab FILENAME via SaveFile.
Buffer contents are intentionally ignored — collab already holds
the authoritative document via ops.  If the current buffer is not
the live collab-monitored buffer for FILENAME, signal a file-error
rather than silently saving stale remote state."
  (let ((expanded (expand-file-name filename)))
    (with-parsed-tramp-file-name expanded nil
      (when (not (tramp-collab--connected-p host))
        (signal 'file-error
                (list "Collab host not connected" host expanded)))
      (let ((file-desc (tramp-collab--vec-file-desc v)))
        (when (not (and (bound-and-true-p collab-monitored-mode)
                        (boundp 'collab--file)
                        collab--file
                        (equal (plist-get collab--file :hostId)
                               (plist-get file-desc :hostId))
                        (equal (plist-get collab--file :project)
                               (plist-get file-desc :project))
                        (equal (plist-get collab--file :file)
                               (plist-get file-desc :file))))
          (signal 'file-error
                  (list "write-region without collab monitor" expanded)))
        (collab--save-file-req file-desc)
        (when visit
          (when (stringp visit) (setq buffer-file-name visit))
          (set-buffer-modified-p nil)
          (set-visited-file-modtime))
        (tramp-flush-file-properties v localname)
        (message "Wrote %s" expanded)))))

(defun tramp-collab-handle-make-directory (_dir &optional _parents)
  "No-op stub for collab.  A real MakeDir RPC will replace this."
  nil)

;;;; Backend wiring

(defconst tramp-collab-file-name-handler-alist
  '(;; Custom handlers.
    (file-exists-p . tramp-collab-handle-file-exists-p)
    (file-readable-p . tramp-collab-handle-file-readable-p)
    (file-writable-p . tramp-collab-handle-file-writable-p)
    (file-attributes . tramp-collab-handle-file-attributes)
    (directory-files . tramp-collab-handle-directory-files)
    (directory-files-and-attributes
     . tramp-collab-handle-directory-files-and-attributes)
    (file-name-all-completions
     . tramp-collab-handle-file-name-all-completions)
    (insert-file-contents . tramp-collab-handle-insert-file-contents)
    (write-region . tramp-collab-handle-write-region)
    (make-directory . tramp-collab-handle-make-directory)

    ;; Generic Tramp handlers.
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
    (file-name-case-insensitive-p . tramp-handle-file-name-case-insensitive-p)
    (file-name-directory . tramp-handle-file-name-directory)
    (file-name-nondirectory . tramp-handle-file-name-nondirectory)
    (file-regular-p . tramp-handle-file-regular-p)
    (file-remote-p . tramp-handle-file-remote-p)
    (file-truename . tramp-handle-file-truename)
    (file-name-completion . tramp-handle-file-name-completion)
    (find-backup-file-name . tramp-handle-find-backup-file-name)
    (insert-directory . tramp-handle-insert-directory)
    (make-auto-save-file-name . tramp-handle-make-auto-save-file-name)
    (set-visited-file-modtime . tramp-handle-set-visited-file-modtime)
    (verify-visited-file-modtime . tramp-handle-verify-visited-file-modtime)

    ;; Explicitly unsupported — return nil so callers don’t break.
    (exec-path . ignore)
    (file-acl . ignore)
    (file-local-copy . ignore)
    (file-locked-p . ignore)
    (file-notify-add-watch . ignore)
    (file-selinux-context . ignore)
    (file-symlink-p . ignore)
    (lock-file . ignore)
    (make-process . ignore)
    (make-symbolic-link . ignore)
    (process-file . ignore)
    (set-file-modes . ignore)
    (set-file-times . ignore)
    (shell-command . ignore)
    (start-file-process . ignore)
    (unlock-file . ignore))
  "Alist of handler functions for the collab Tramp backend.
Operations not listed here fall through to the default Emacs
behavior via ‘tramp-run-real-handler’ (typically signaling a
file-error, e.g. for delete-file/copy-file/rename-file).")

;;;###tramp-autoload
(defsubst tramp-collab-file-name-p (vec-or-filename)
  "Check whether VEC-OR-FILENAME is handled by the collab backend."
  (when-let* ((vec (tramp-ensure-dissected-file-name vec-or-filename)))
    (string= (tramp-file-name-method vec) tramp-collab-method)))

;;;###tramp-autoload
(defun tramp-collab-file-name-handler (operation &rest args)
  "Invoke the collab backend handler for OPERATION with ARGS."
  (if-let* ((fn (assoc operation tramp-collab-file-name-handler-alist)))
      (save-match-data (apply (cdr fn) args))
    (tramp-run-real-handler operation args)))

;;;; Host completion

(defun tramp-collab--parse-hosts (&optional _filename)
  "Return collab hosts as a list of (USER HOST) tuples for completion.
The list is the union of ‘collab-host-alist’ keys and the local
host id when known.  Returns nil if collab-mode hasn’t loaded yet."
  (when (boundp 'collab-host-alist)
    (let ((hosts (mapcar #'car collab-host-alist)))
      (when (and (boundp 'collab--my-host-id) collab--my-host-id)
        (cl-pushnew collab--my-host-id hosts :test #'equal))
      (mapcar (lambda (h) (list nil h)) hosts))))

;; Register directly: tramp.el has already fully loaded (because we
;; (require 'tramp) at the top of this file), so the tramp--with-startup
;; hook has already fired and adding to it now would be a no-op.
(add-to-list 'tramp-methods `(,tramp-collab-method))
(tramp-register-foreign-file-name-handler
 #'tramp-collab-file-name-p #'tramp-collab-file-name-handler)
(tramp-set-completion-function
 tramp-collab-method '((tramp-collab--parse-hosts "")))

(provide 'tramp-collab)

;;; tramp-collab.el ends here
