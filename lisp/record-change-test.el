;;; record-change-test.el --- Testing framework for collab-mode edit recording -*- lexical-binding: t; -*-

;;; Commentary:
;; This file provides a testing framework for the edit recording
;; functionality in collab-mode.el. It creates two buffers (work and mirror),
;; executes commands from transcript files, and verifies that recorded
;; operations produce identical results when applied.

;;; Code:

(require 'collab-mode)

;;; Variables

(defvar collab-test--work-buffer nil
  "The work buffer where commands are executed.")

(defvar collab-test--mirror-buffer nil
  "The mirror buffer where recorded ops are applied.")

(defvar collab-test--transcript-buffer nil
  "Buffer containing the transcript being processed.")

(defvar collab-test-delay 0.01
  "Delay in seconds after each key press during test execution.")

;;; Test minor mode

(defun collab-test--post-command ()
  "Test version of collab--post-command that applies ops directly to mirror."
  (when collab--ops-current-command
    ;; Apply the recorded ops to mirror buffer
    (collab-test--apply-ops-to-mirror (nreverse collab--ops-current-command))
    (setq collab--ops-current-command nil)))

(defun collab-test--post-command-hasty ()
  "Test version of collab--post-command-hasty that applies ops directly to mirror."
  (when collab--ops-current-command
    (collab-test--apply-ops-to-mirror (nreverse collab--ops-current-command))
    (setq collab--ops-current-command nil)))

(define-minor-mode collab-test-mode
  "Test mode for collab-mode edit recording.
This mode sets up the same hooks as `collab-monitored-mode' but
instead of sending operations over the network, it applies them
to a mirror buffer for testing."
  :global nil
  (if collab-test-mode
      (progn
        ;; Use the actual collab-mode hooks for recording changes
        (add-hook 'before-change-functions #'collab--before-change 0 t)
        (add-hook 'after-change-functions #'collab--after-change 0 t)
        (add-hook 'pre-command-hook #'collab--pre-command 0 t)
        ;; Use our test post-command hook instead of collab's
        (if collab-hasty-p
            (add-hook 'post-command-hook
                      #'collab-test--post-command-hasty 0 t)
          (add-hook 'post-command-hook
                    #'collab-test--post-command 0 t))
        ;; Initialize collab-mode variables
        (setq-local collab--pending-ops nil)
        (setq-local collab--ops-current-command nil)
        (setq-local collab--group-seq 1)
        (setq-local collab--changing-region nil))
    ;; Disable - remove all hooks
    (remove-hook 'before-change-functions #'collab--before-change t)
    (remove-hook 'after-change-functions #'collab--after-change t)
    (remove-hook 'pre-command-hook #'collab--pre-command t)
    (remove-hook 'post-command-hook #'collab-test--post-command t)
    (remove-hook 'post-command-hook #'collab-test--post-command-hasty t)))

;;; Operation processing

(defun collab-test--apply-ops-to-mirror (ops)
  "Apply OPS to the mirror buffer."
  (when (and ops collab-test--mirror-buffer
             (buffer-live-p collab-test--mirror-buffer))
    (with-current-buffer collab-test--mirror-buffer
      (let ((inhibit-modification-hooks t)
            (collab--inhibit-hooks t))
        (save-excursion
          (dolist (op ops)
            (collab-test--apply-single-op op)))))))

(defun collab-test--apply-single-op (op)
  "Apply a single operation OP to current buffer.
OP is in the format (ins|del POS STR GROUP-SEQ) as created by collab--after-change."
  (pcase op
    (`(ins ,pos ,str ,_group-seq)
     (goto-char pos)
     (insert str))
    (`(del ,pos ,str ,_group-seq)
     (delete-region pos (+ pos (length str))))))

;;; Transcript parsing

(defun collab-test--parse-transcript (file)
  "Parse transcript FILE and return (headers commands initial-content)."
  (with-temp-buffer
    (insert-file-contents file)
    (goto-char (point-min))
    (let ((headers (collab-test--parse-headers))
          (commands (collab-test--parse-commands))
          (initial-content (collab-test--parse-initial-content)))
      (list headers commands initial-content))))

(defun collab-test--parse-headers ()
  "Parse transcript headers until double newline."
  (let ((headers '())
        (header-end (save-excursion
                      (re-search-forward "\n\n" nil t))))
    (when header-end
      (save-restriction
        (narrow-to-region (point) header-end)
        (while (re-search-forward "^\\([A-Z-]+\\): *\\(.+\\)$" nil t)
          (push (cons (match-string 1) (match-string 2)) headers))))
    (goto-char (or header-end (point)))
    (nreverse headers)))

(defun collab-test--parse-commands ()
  "Parse commands until double newline."
  (let ((commands '())
        (commands-end (save-excursion
                        (re-search-forward "\n\n" nil t))))
    (when commands-end
      (save-restriction
        (narrow-to-region (point) commands-end)
        (while (not (eobp))
          (cond
           ((looking-at "^INDENT *$")
            (push '(indent) commands)
            (forward-line))
           ((looking-at "^INSERT \\(.+\\)$")
            (let ((text (match-string 1)))
              (setq text (replace-regexp-in-string "\\\\t" "\t" text))
              (setq text (replace-regexp-in-string "\\\\n" "\n" text))
              (push `(insert ,text) commands))
            (forward-line))
           ((looking-at "^DELETE \\([+-]?[0-9]+\\)$")
            (push `(delete ,(string-to-number (match-string 1))) commands)
            (forward-line))
           ((looking-at "^MOVE \\([+-]?[0-9]+\\)$")
            (push `(move ,(string-to-number (match-string 1))) commands)
            (forward-line))
           (t (forward-line))))))
    (goto-char (or commands-end (point)))
    (nreverse commands)))

(defun collab-test--parse-initial-content ()
  "Return the initial content after headers and commands."
  (buffer-substring-no-properties (point) (point-max)))

;;; Command execution

(defun collab-test--execute-command (cmd)
  "Execute a single transcript command CMD using execute-kbd-macro."
  (pcase cmd
    (`(indent)
     ;; Press TAB key
     (execute-kbd-macro (kbd "TAB"))
     (sit-for collab-test-delay))
    (`(insert ,text)
     ;; Type the text character by character as if user typed it
     (dolist (char (string-to-list text))
       (execute-kbd-macro (string char))
       (sit-for collab-test-delay)))
    (`(delete ,n)
     ;; Delete characters using DEL or <deletechar>
     (cond
      ;; Delete backward (like backspace/DEL key)
      ((> n 0)
       (dotimes (_ n)
         (execute-kbd-macro (kbd "DEL"))
         (sit-for collab-test-delay)))
      ;; Delete forward (like delete key)
      (t
       (dotimes (_ (- n))
         (execute-kbd-macro (kbd "<deletechar>"))
         (sit-for collab-test-delay)))))
    (`(move ,n)
     ;; Move cursor using C-f/C-b
     (cond
      ((> n 0)
       ;; Move forward
       (dotimes (_ n)
         (execute-kbd-macro (kbd "C-f"))))
      ((< n 0)
       ;; Move backward
       (dotimes (_ (- n))
         (execute-kbd-macro (kbd "C-b"))))))))

;;; Test runner

(defun collab-test--execute-commands-async (commands callback)
  "Execute COMMANDS asynchronously, then call CALLBACK when done.
This uses timers to ensure proper event processing between commands."
  (if commands
      (let ((cmd (car commands))
            (rest (cdr commands)))
        ;; Execute current command
        (collab-test--execute-command cmd)
        ;; Schedule the rest
        (run-with-timer collab-test-delay nil
                        (lambda ()
                          (collab-test--execute-commands-async rest callback))))
    ;; No more commands, call the callback
    (when callback
      (run-with-timer collab-test-delay nil callback))))

(defun collab-test--setup-buffers (initial-content)
  "Setup work and mirror buffers with INITIAL-CONTENT."
  (setq collab-test--work-buffer
        (get-buffer-create "*collab-test-work*"))
  (setq collab-test--mirror-buffer
        (get-buffer-create "*collab-test-mirror*"))
  
  (with-current-buffer collab-test--work-buffer
    (erase-buffer)
    (insert initial-content)
    (goto-char (point-min)))
  
  (with-current-buffer collab-test--mirror-buffer
    (erase-buffer)
    (insert initial-content)
    (goto-char (point-min))))

(defun collab-test--apply-headers (headers)
  "Apply transcript HEADERS to work buffer."
  (with-current-buffer collab-test--work-buffer
    (dolist (header headers)
      (pcase header
        (`("MAJOR-MODE" . ,mode)
         (when (fboundp (intern mode))
           (funcall (intern mode))))
        (`("MINOR-MODE" . ,mode)
         (when (fboundp (intern mode))
           (funcall (intern mode) 1)))
        (`("HASTE" . ,value)
         (setq-local collab-hasty-p (string= value "t")))))))

(defun collab-test--compare-buffers ()
  "Compare work and mirror buffers, return t if identical."
  (string= (with-current-buffer collab-test--work-buffer
             (buffer-substring-no-properties (point-min) (point-max)))
           (with-current-buffer collab-test--mirror-buffer
             (buffer-substring-no-properties (point-min) (point-max)))))

;;; Public interface

(defun collab-test-run-transcript (transcript-file)
  "Run a single TRANSCRIPT-FILE and verify results."
  (interactive "fTranscript file: ")
  (let* ((parsed (collab-test--parse-transcript transcript-file))
         (headers (nth 0 parsed))
         (commands (nth 1 parsed))
         (initial-content (nth 2 parsed)))
    
    ;; Setup buffers
    (collab-test--setup-buffers initial-content)
    
    ;; Apply headers and enable test mode
    (collab-test--apply-headers headers)
    
    ;; Show buffers side-by-side when running interactively
    (when (called-interactively-p 'any)
      (delete-other-windows)
      (switch-to-buffer collab-test--work-buffer)
      (split-window-horizontally)
      (other-window 1)
      (switch-to-buffer collab-test--mirror-buffer)
      (other-window 1))

    (select-window (get-buffer-window collab-test--work-buffer))
    (collab-test-mode)

    ;; Execute commands asynchronously
    (collab-test--execute-commands-async
     commands
     (lambda ()
       (if (collab-test--compare-buffers)
           (message "Test PASSED")
         (message "Test FAILED"))))))

(defun collab-test-run-all ()
  "Run all transcript files in the test directory."
  (interactive)
  (let ((test-dir (expand-file-name "transcripts"
                                    (file-name-directory
                                     (or load-file-name buffer-file-name))))
        (passed 0)
        (failed 0))
    (dolist (file (directory-files test-dir t "\\.transcript$"))
      (if (collab-test-run-transcript file)
          (cl-incf passed)
        (cl-incf failed)))
    (message "Tests complete: %d passed, %d failed" passed failed)))

(defun collab-test-spawn-emacs (transcript-file)
  "Run TRANSCRIPT-FILE test in a new Emacs instance."
  (interactive "fTranscript file: ")
  (let ((test-file (expand-file-name "record-change-test.el"
                                     (file-name-directory
                                      (or load-file-name buffer-file-name)))))
    (start-process "collab-test-emacs" "*collab-test-output*"
                   (expand-file-name invocation-name invocation-directory)
                   "-L" (file-name-directory test-file)
                   "-l" test-file
                   "--eval"
                   (format "(collab-test-run-transcript %S)"
                           transcript-file))))

(provide 'record-change-test)
;;; record-change-test.el ends here
