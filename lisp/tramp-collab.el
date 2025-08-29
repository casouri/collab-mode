;;; tramp-collab.el --- TRAMP backend for collab-mode  -*- lexical-binding: t; -*-

;; Author: Yuan Fu <casouri@gmail.com>
;; Keywords: comm, files

;;; Commentary:

;; This file provides a TRAMP backend for collab-mode, allowing access to
;; remote files shared via collab-mode using TRAMP file name syntax:
;;
;; /collab:<host-id>/<type>/<doc-id or project>/<path>
;;
;; Where:
;; - host-id: The host identifier
;; - type: "p" for project, "f" for file
;; - doc-id or project: Document ID for files, project name for projects
;; - path: Path within a project (only for projects)

;;; Code:

(require 'tramp)
(require 'collab-mode)

;;; Method definition

;;;###tramp-autoload
(defconst tramp-collab-method "collab"
  "Method to access collab-mode shared files.")

;;;###tramp-autoload
(tramp--with-startup
 (add-to-list 'tramp-methods
              `(,tramp-collab-method
                ;; No login program needed - we use collab-mode's connection
                (tramp-login-program nil)
                (tramp-login-args nil)
                (tramp-remote-shell nil)
                (tramp-remote-shell-args nil))))

;;; File name parsing

(defun tramp-collab-file-name-p (filename)
  "Return t if FILENAME is a collab TRAMP file name."
  (and (stringp filename)
       (string-match-p (concat "^/" tramp-collab-method ":") filename)))

(defun tramp-collab-parse-file-name (filename)
  "Parse a collab TRAMP FILENAME.
Returns a plist (:host-id :type :id :path)."
  (when (tramp-collab-file-name-p filename)
    (let* ((localname (tramp-file-name-localname (tramp-dissect-file-name filename)))
           (parts (split-string localname "/" t)))
      (when (>= (length parts) 2)
        (let ((host-id (nth 0 parts))
              (type (nth 1 parts))
              (id (nth 2 parts))
              (path (when (> (length parts) 3)
                      (string-join (nthcdr 3 parts) "/"))))
          (list :host-id host-id
                :type type
                :id id
                :path path))))))

(defun tramp-collab-make-file-desc (parsed-name)
  "Create a file descriptor from PARSED-NAME.
PARSED-NAME is the result of `tramp-collab-parse-file-name'."
  (let ((type (plist-get parsed-name :type))
        (id (plist-get parsed-name :id))
        (path (plist-get parsed-name :path)))
    (cond
     ((equal type "f")
      (collab--make-file-desc "file" id))
     ((equal type "p")
      (if path
          (collab--make-file-desc "projectFile" nil id path)
        (collab--make-file-desc "project" id)))
     (t (error "Invalid collab file type: %s" type)))))

;;; Connection management

(defun tramp-collab-get-signaling-server (host-id)
  "Get the signaling server for HOST-ID.
Returns the signaling server from collab-host-alist, or the default."
  (let ((host-config (alist-get host-id collab-host-alist nil nil #'equal)))
    (if host-config
        (car host-config)  ; First element is signaling server
      collab-default-signaling-server)))

(defun tramp-collab-get-credential (host-id)
  "Get the credential for HOST-ID.
Returns the credential from collab-host-alist, or empty string."
  (let ((host-config (alist-get host-id collab-host-alist nil nil #'equal)))
    (if host-config
        (cadr host-config)  ; Second element is credential
      "")))

(defun tramp-collab-ensure-connection ()
  "Ensure collab process connection is established."
  (unless (and collab--jsonrpc-connection
               (jsonrpc-running-p collab--jsonrpc-connection))
    (collab--connect-process)))

;;; File operations

(defun tramp-collab-handle-file-exists-p (filename)
  "Check if FILENAME exists."
  (condition-case nil
      (let* ((parsed (tramp-collab-parse-file-name filename))
             (host-id (plist-get parsed :host-id))
             (signaling-server (tramp-collab-get-signaling-server host-id))
             (credential (tramp-collab-get-credential host-id))
             (file-desc (tramp-collab-make-file-desc parsed)))
        (tramp-collab-ensure-connection)
        (pcase (plist-get file-desc :type)
          ;; Top-level file
          ("file"
           ;; Get top-level entries by passing nil
           (let ((resp (collab--list-files-req nil host-id signaling-server credential)))
             (cl-some (lambda (entry)
                        (equal (plist-get (plist-get entry :file) :id) 
                               (plist-get file-desc :id)))
                      (plist-get resp :files))))
          
          ;; Top-level project
          ("project"
           ;; Get top-level entries by passing nil
           (let ((resp (collab--list-files-req nil host-id signaling-server credential)))
             (cl-some (lambda (entry)
                        (let ((entry-desc (plist-get entry :file)))
                          (and (equal (plist-get entry-desc :type) "project")
                               (equal (plist-get entry-desc :id) 
                                      (plist-get file-desc :id)))))
                      (plist-get resp :files))))
          
          ;; File within a project
          ("projectFile"
           (let* ((path (plist-get parsed :path))
                  (path-parts (split-string path "/" t))
                  (parent-path (if (> (length path-parts) 1)
                                   (string-join (butlast path-parts) "/")
                                 nil))
                  (filename-part (car (last path-parts)))
                  (parent-desc (if parent-path
                                   (collab--make-file-desc "projectFile" nil 
                                                           (plist-get file-desc :project) parent-path)
                                 (collab--make-file-desc "project" (plist-get file-desc :project))))
                  (resp (collab--list-files-req parent-desc host-id signaling-server credential)))
             ;; Check if filename exists in parent directory listing
             (cl-some (lambda (entry)
                        (equal (plist-get entry :filename) filename-part))
                      (plist-get resp :files))))
          
          (_ nil)))
    (error nil)))

(defun tramp-collab-handle-file-readable-p (filename)
  "Check if FILENAME is readable."
  ;; In collab-mode, if a file exists, it's readable
  (tramp-collab-handle-file-exists-p filename))

(defun tramp-collab-handle-file-writable-p (filename)
  "Check if FILENAME is writable."
  ;; In collab-mode, if a file exists, it's writable
  (tramp-collab-handle-file-exists-p filename))

(defun tramp-collab-handle-file-directory-p (filename)
  "Check if FILENAME is a directory."
  (condition-case nil
      (let* ((parsed (tramp-collab-parse-file-name filename))
             (host-id (plist-get parsed :host-id))
             (signaling-server (tramp-collab-get-signaling-server host-id))
             (credential (tramp-collab-get-credential host-id))
             (file-desc (tramp-collab-make-file-desc parsed)))
        (when parsed
          (tramp-collab-ensure-connection)
          (pcase (plist-get file-desc :type)
            ;; Top-level file is not a directory
            ("file" nil)
            
            ;; Top-level project is a directory
            ("project" t)
            
            ;; Project file - need to check parent directory listing
            ("projectFile"
             (let* ((path (plist-get parsed :path))
                    (path-parts (split-string path "/" t))
                    (parent-path (if (> (length path-parts) 1)
                                     (string-join (butlast path-parts) "/")
                                   nil))
                    (filename-part (car (last path-parts)))
                    (parent-desc (if parent-path
                                     (collab--make-file-desc "projectFile" nil 
                                                             (plist-get parsed :id) parent-path)
                                   (collab--make-file-desc "project" (plist-get parsed :id))))
                    (resp (collab--list-files-req parent-desc host-id signaling-server credential)))
               ;; Check if the entry is a directory
               (cl-some (lambda (entry)
                          (and (equal (plist-get entry :filename) filename-part)
                               (plist-get entry :is_directory)))
                        (plist-get resp :files))))
            
            (_ nil))))
    (error nil)))

(defun tramp-collab-handle-directory-files (directory &optional full match nosort)
  "Return a list of names of files in DIRECTORY.
If FULL is non-nil, return absolute file names.
If MATCH is non-nil, only return files matching this regexp.
If NOSORT is non-nil, the list is not sorted."
  (let* ((parsed (tramp-collab-parse-file-name directory))
         (host-id (plist-get parsed :host-id))
         (signaling-server (tramp-collab-get-signaling-server host-id))
         (credential (tramp-collab-get-credential host-id))
         (dir-desc (when (equal (plist-get parsed :type) "p")
                     (tramp-collab-make-file-desc parsed))))
    (tramp-collab-ensure-connection)
    (condition-case nil
        (let* ((resp (collab--list-files-req dir-desc host-id signaling-server credential))
               (files (plist-get resp :files))
               (names (mapcar (lambda (entry)
                                (let ((filename (plist-get entry :filename)))
                                  (if full
                                      (concat directory "/" filename)
                                    filename)))
                              files)))
          (when match
            (setq names (cl-remove-if-not
                         (lambda (name) (string-match-p match name))
                         names)))
          (if nosort
              names
            (sort names #'string<)))
      (error nil))))

(defun tramp-collab-handle-insert-file-contents (filename &optional visit beg end replace)
  "Insert contents of FILENAME into current buffer.
VISIT, BEG, END, and REPLACE are ignored - always replaces whole buffer."
  ;; Disable collab-monitored-mode before replacing content
  (when collab-monitored-mode
    (collab-monitored-mode -1))
  
  (let* ((parsed (tramp-collab-parse-file-name filename))
         (host-id (plist-get parsed :host-id))
         (file-desc (tramp-collab-make-file-desc parsed)))
    (tramp-collab-ensure-connection)
    (let* ((resp (collab--open-file-req file-desc host-id "Open"))
           (content (plist-get resp :content))
           (size (length content))
           (site-id (plist-get resp :siteId))
           (doc-id (plist-get resp :docId)))
      ;; Always replace the whole buffer
      (delete-region (point-min) (point-max))
      (insert content)
      (goto-char (point-min))
      
      ;; Enable collab-mode for this buffer
      (collab--enable doc-id host-id site-id)
      
      (when visit
        (set-visited-file-name filename)
        (set-buffer-modified-p nil))
      
      (list filename size))))

(defun tramp-collab-handle-write-region (start end filename &optional append visit lockname mustbenew)
  "Write region from START to END to FILENAME."
  ;; In collab-mode, edits are transmitted in real-time, so
  ;; write-region is a no-op. We just need to handle the visit flag.
  (when visit
    (set-visited-file-name filename)
    (set-buffer-modified-p nil))
  ;; Return nil to indicate success.
  nil)

(defun tramp-collab-handle-delete-file (filename &optional trash)
  "Delete FILENAME.
If TRASH is non-nil, move to trash instead (not supported in collab-mode)."
  (let* ((parsed (tramp-collab-parse-file-name filename))
         (host-id (plist-get parsed :host-id))
         (doc-id (plist-get parsed :id)))
    (tramp-collab-ensure-connection)
    (collab--delete-file-req doc-id host-id)))

(defun tramp-collab-handle-save-buffer (&optional arg)
  "Save current buffer to its file.
ARG is passed to the save operation."
  (let* ((filename (buffer-file-name))
         (parsed (tramp-collab-parse-file-name filename))
         (host-id (plist-get parsed :host-id))
         (doc-id (plist-get parsed :id)))
    (tramp-collab-ensure-connection)
    (collab--save-file-req doc-id host-id)
    (set-buffer-modified-p nil)))

(defun tramp-collab-handle-rename-file (filename newname &optional ok-if-already-exists)
  "Rename FILENAME to NEWNAME.
If OK-IF-ALREADY-EXISTS is non-nil, overwrite existing file.
Only works for files within the same project."
  (let* ((parsed-old (tramp-collab-parse-file-name filename))
         (parsed-new (tramp-collab-parse-file-name newname))
         (old-file-desc (tramp-collab-make-file-desc parsed-old))
         (new-file-desc (tramp-collab-make-file-desc parsed-new)))
    (tramp-collab-ensure-connection)
    ;; Only support renaming within the same project
    (cond
     ;; Both must be projectFile type
     ((and (equal (plist-get old-file-desc :type) "projectFile")
           (equal (plist-get new-file-desc :type) "projectFile"))
      (let ((old-project (plist-get old-file-desc :project))
            (new-project (plist-get new-file-desc :project))
            (old-path (plist-get old-file-desc :file))
            (new-path (plist-get new-file-desc :file))
            (host-id (plist-get parsed-old :host-id)))
        (if (equal old-project new-project)
            ;; Same project - can rename
            (progn
              (when (and (not ok-if-already-exists)
                         (tramp-collab-handle-file-exists-p newname))
                (error "File already exists: %s" newname))
              (collab--move-file-req old-project old-path new-path host-id))
          (error "Cannot rename files across different projects"))))
     ;; Cannot rename top-level files or projects
     (t
      (error "Can only rename files within projects, not top-level items")))))

;;; File name handler

;;;###tramp-autoload
(defconst tramp-collab-file-name-handler-alist
  '((file-exists-p . tramp-collab-handle-file-exists-p)
    (file-readable-p . tramp-collab-handle-file-readable-p)
    (file-writable-p . tramp-collab-handle-file-writable-p)
    (file-directory-p . tramp-collab-handle-file-directory-p)
    (directory-files . tramp-collab-handle-directory-files)
    (insert-file-contents . tramp-collab-handle-insert-file-contents)
    (write-region . tramp-collab-handle-write-region)
    (delete-file . tramp-collab-handle-delete-file)
    (rename-file . tramp-collab-handle-rename-file)
    ;; Operations we explicitly don't support
    (file-attributes . ignore)
    (file-modes . ignore)
    (set-file-modes . ignore)
    (file-selinux-context . ignore)
    (set-file-selinux-context . ignore)
    (file-acl . ignore)
    (set-file-acl . ignore)
    (copy-file . ignore)
    (copy-directory . ignore)
    (make-directory . ignore)
    (make-directory-internal . ignore)
    (delete-directory . ignore)
    (file-locked-p . ignore)
    (lock-file . ignore)
    (unlock-file . ignore)
    (file-executable-p . ignore)
    (file-accessible-directory-p . ignore)
    (file-ownership-preserved-p . ignore)
    (file-newer-than-file-p . ignore)
    (file-symlink-p . ignore)
    (make-symbolic-link . ignore)
    (file-truename . ignore)
    (file-equal-p . ignore)
    (file-in-directory-p . ignore)
    (file-name-case-insensitive-p . ignore)
    (find-backup-file-name . ignore)
    (make-auto-save-file-name . ignore)
    (file-remote-p . ignore)
    (file-local-copy . ignore)
    (file-notify-add-watch . ignore)
    (file-notify-rm-watch . ignore)
    (file-notify-valid-p . ignore)
    (start-file-process . ignore)
    (make-process . ignore)
    (process-file . ignore)
    (shell-command . ignore)
    (temporary-file-directory . ignore)
    (exec-path . ignore)
    (load . ignore)
    (substitute-in-file-name . ignore)
    (unhandled-file-name-directory . ignore)
    (vc-registered . ignore)
    (verify-visited-file-modtime . ignore)
    (set-visited-file-modtime . ignore)
    (set-file-times . ignore)
    (file-regular-p . ignore)
    (file-name-all-completions . ignore)
    (file-name-completion . ignore)
    (add-name-to-file . ignore)
    (make-nearby-temp-file . ignore)
    (expand-file-name . ignore))
  "Alist of handler functions for collab TRAMP method.")

;;;###tramp-autoload
(defun tramp-collab-file-name-handler (operation &rest args)
  "Invoke collab-mode file name handler for OPERATION and ARGS."
  (if-let ((fn (assoc operation tramp-collab-file-name-handler-alist)))
      (save-match-data (apply (cdr fn) args))
    (tramp-run-real-handler operation args)))

;;; Registration

;;;###tramp-autoload
(defun tramp-collab-register ()
  "Register collab TRAMP handlers."
  (tramp-register-foreign-file-name-handler
   'tramp-collab-file-name-p
   'tramp-collab-file-name-handler))

;; Register on load
(tramp-collab-register)

(provide 'tramp-collab)

;;; tramp-collab.el ends here
