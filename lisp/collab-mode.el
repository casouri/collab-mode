;;; collab-mode.el --- Collaborative editing mode  -*- lexical-binding: t; -*-

;; Author: Yuan Fu <casouri@gmail.com>

;;; This file is NOT part of GNU Emacs

;;; Commentary:

;;; Code:

(require 'jsonrpc)
(require 'text-property-search)
(require 'rmc)
(require 'icons)
(require 'pulse)

;;; Custom options

(defgroup collab
  ()
  "Collaboration mode."
  :group 'editting)

(defcustom collab-display-name nil
  "The display name of the user."
  :type 'string)

(defcustom collab-local-host-config nil
  "A list storing configuration for the local host.
The list should be (HOST-ID SIGNALING-SERVER-ADDR)."
  :type '(list string))

;; Generated with seaborn.color_palette("colorblind").as_hex()
(defcustom collab-cursor-colors
  '( "#0173b2" "#de8f05" "#029e73" "#d55e00" "#cc78bc" "#ca9161"
     "#fbafe4" "#949494" "#ece133" "#56b4e9")
  "Cursor color ring."
  :type '(list string))

(defcustom collab-send-ops-delay 0.5
  "Collab sends ops when Emacs is idle for this much time."
  :type 'number)

(defcustom collab-receive-ops-delay 1
  "Delay for getting arrived remote ops.
When there are remote ops, collab waits for this long before
grabbing them and applying them to buffer. We don’t need to get
remote ops immediately since whenever user generates new ops, we
send our ops and get remote ops in one go."
  :type 'number)

(defface collab-success-background
  '((t . (:inherit diff-refine-added :extend t)))
  "The highlight used for indicating a success.
Collab uses this face to flash the screen when connected to a doc.")

(defface collab-failure-background
  '((t . (:inherit diff-refine-removed :extend t)))
  "The highlight used for indicating a failure.
Collab uses this face to flash the screen when disconnected from
a doc.")

(defvar collab-rpc-timeout 3
  "Timeout in seconds to wait for connection to collab process.")

(defvar collab-connection-timeout 30
  "Timeout in seconds to wait for connection to remote hosts.
Connecting to remote hosts might be slow since hosts might need
to perform NAT traversal.")

(defvar collab-command "~/p/collab/target/debug/collab"
  "Path to the collab process executable.")

(defvar collab-default-signaling-server "wss://signal.collab-mode.org"
  "Default signaling server’s address.

Signaling server is for NAT-traversal and host authentication.")

(defvar collab-connection-type '(socket 7701)
  "How to connect to the collab process.
The value can be ‘pipe’, meaning use stdio for connection,
or (socket PORT), meaning using a socket connection, and PORT
should be the port number.")

(defvar collab-host-alist nil
  "An alist of host configurations.
Each key is HOST-ID, each value is (SIGNALING-SERVER-ADDR
CREDENTIAL).")

(defvar collab-hasty-p nil
  "If t, buffer changes are sent to collab process immediately.
For changes to this variable to take affect, you need to
reconnect to the doc.")

(defvar collab--verbose nil
  "If non-nil, print debugging information.")

;;; Generic helpers

;; https://nullprogram.com/blog/2010/05/11/
(defun collab--uuid ()
  "Return a newly generated UUID."
  (let ((str (md5 (format "%s%s%s%s%s%s%s%s%s%s"
                          (user-uid)
                          (emacs-pid)
                          (system-name)
                          (user-full-name)
                          user-mail-address
                          (current-time)
                          (emacs-uptime)
                          (garbage-collect)
                          (random)
                          (recent-keys)))))
    (format "%s-%s-3%s-%s-%s"
            (substring str 0 8)
            (substring str 8 12)
            (substring str 13 16)
            (substring str 16 20)
            (substring str 20 32))))

(defun collab--decode-color (color)
  "Convert COLOR in ”#RRGGBB” format to (R G B)."
  (mapcar (lambda (color)
            (/ color 256))
          (or (color-values color) (color-values "white"))))

(defun collab--encode-color (color)
  "Convert COLOR in (R G B) format to ”#RRGGBB”."
  (format "#%.2x%.2x%.2x" (nth 0 color) (nth 1 color) (nth 2 color)))

(defun collab--blend-color (base above alpha)
  "Return a color made of ABOVE with alpha ALPHA placed above BASE.
Both COLOR’S are like ”#RRGGBB”, ALPHA is a float between 0 and 1."
  (collab--encode-color
   (cl-labels ((comp (base above alpha)
                 (+ (* base (- 1 alpha)) (* above alpha)))
               (bound (color) (cond ((> color 255) 255)
                                    ((< color 0) 0)
                                    (t color))))
     (let* ((color-base (collab--decode-color base))
            (color-above (collab--decode-color above)))
       (cl-loop for base in color-base
                for above in color-above
                collect (bound (comp base above alpha)))))))

(defun collab--fairy (format-string &rest args)
  "Format string, and append a fairy and the front.
Behaves like ‘format’ regarding FORMAT-STRING and ARGS. It also
substitutes command keys like docstring, see
‘substitute-command-keys’."
  (substitute-command-keys
   (apply #'format (concat (if (char-displayable-p ?🧚) "🧚 " "* ")
                           format-string)
          args)))

;;; Icons

(defvar collab--load-directory
  (file-name-directory (or load-file-name buffer-file-name))
  "Directory in which collab.el resides.")

(define-icon collab-status-on nil
  `((image ,(concat collab--load-directory "/dot_medium_16.svg")
           :face success
           :height (0.9 . em)
           :ascent 83)
    (symbol "•")
    (text "*"))
  "Icon for collab on status indicator."
  :version "30.1"
  :help-echo "ON")

(define-icon collab-status-off nil
  `((image ,(concat collab--load-directory "/dot_medium_16.svg")
           :face error
           :height (0.9 . em)
           :ascent 83)
    (symbol "•")
    (text "*"))
  "Icon for collab off status indicator."
  :version "30.1"
  :help-echo "OFF")

;;; Error handling

(defvar collab--error-code-alist
  '((-32700 . ParseError)
    (-32600 . InvalidRequest)
    (-32601 . MethodNotFound)
    (-32602 . InvalidParams)
    (-32603 . InternalError)

    (-32002 . NotInitialized)

    (103 . DocFatal)
    (104 . PermissionDenied)
    (105 . IoError)
    (113 . BadRequest))
  "An alist of JSONRPC error codes.")

(defun collab--error-code-jsonrpc-error-p (code)
  "Return non-nil if CODE represents a JSONRPC error."
  (<= -32768 code -32000))

(defmacro collab--catch-error (msg &rest body)
  "Execute BODY, catch jsonrpc errors.
If there’s an error, print “collab MSG: ERROR-DESCRIPTION”,
and append the error to collab error buffer (*collab errors*).
MSG should be something like “can’t do xxx”."
  (declare (indent 1) (debug (sexp &rest form)))
  (let ((err (gensym)))
    `(condition-case ,err
         (progn
           ,@body)
       ((debug error)
        (when collab-monitored-mode
          (collab--disable))
        (display-warning 'collab
                         (format "collab %s: %s" ,msg ,err))))))

(defvar collab--jsonrpc-connection)
(defvar collab--doc-host)
(defun collab--check-precondition ()
  "Check ‘collab--jsonrpc-connection’ and ‘collab--doc-host’.

If they are invalid, turn off ‘collab-monitored-mode’ and raise
an error. This function should be called before any interactive
command that acts on a collab-monitored buffer."
  (unless collab--jsonrpc-connection
    (collab-monitored-mode -1)
    (display-warning 'collab "Connection to collab host broke"))
  (unless collab--doc-host
    (collab-monitored-mode -1)
    (display-warning
     'collab "Current buffer doesn’t have a doc id or host id")))


;;; Cursor tracking

(defvar-local collab--cursor-ov-alist nil
  "An alist mapping user’s site id to cursor overlays.")

(defvar-local collab--my-site-id nil
  "My site-id.")

(defvar collab--sync-cursor-timer-table
  (make-hash-table :test #'equal)
  "Hash table mapping (doc-id . host-id) to sync cursor timers.")

(defun collab--move-cursor (site-id pos &optional mark)
  "Move user (SITE-ID)’s cursor overlay to POS.
If MARK non-nil, show active region."
  (when (and (not (eq site-id collab--my-site-id))
             (<= (point-min) pos (point-max)))
    (save-restriction
      (widen)
      (let* ((idx (mod site-id
                       (length collab-cursor-colors)))
             (color (nth idx collab-cursor-colors))
             (region-color (collab--blend-color
                            (face-attribute 'default :background)
                            color 0.2))
             (face `(:background ,color))
             (region-face `(:background ,region-color))
             (ov (or (alist-get site-id collab--cursor-ov-alist)
                     (let ((ov (make-overlay (min (1+ pos) (point-max))
                                             (min (+ pos 2) (point-max))
                                             nil t nil)))
                       (overlay-put ov 'face face)
                       (push (cons site-id ov) collab--cursor-ov-alist)
                       ov))))

        (if (not mark)
            (progn
              (overlay-put ov 'face face)
              (if (eq pos (point-max))
                  (progn
                    (overlay-put ov 'after-string (propertize " " 'face face))
                    (move-overlay ov pos (min (1+ pos) (point-max))))
                (overlay-put ov 'after-string nil)
                (move-overlay ov pos (1+ pos))))

          (move-overlay ov (min pos mark) (max pos mark))
          (overlay-put ov 'face region-face)
          (overlay-put ov 'after-string nil))))))

(defvar collab-monitored-mode)
;; FIXME: Need update.
(defun collab--send-cursor-pos-1 (buffer)
  "Send cursor position of BUFFER to the collab process."
  (when (equal buffer (current-buffer))
    (collab--check-precondition)
    (collab--catch-error "can’t send cursor position to remote"
      (let* ((doc-id (car collab--doc-host))
             (host-id (cdr collab--doc-host)))
        (collab--send-info-req
         doc-id host-id
         (if (region-active-p)
             `( :type "common.pos" :point ,(point)
                :mark ,(mark))
           `(:type "common.pos" :point ,(point))))))

    (remhash collab--doc-host
             collab--sync-cursor-timer-table)))

;; FIXME: Use a global idle time like send-ops.
(defun collab--send-cursor-pos ()
  "Move user (SITE-ID)’s cursor overlay to POS."
  (collab--check-precondition)
  (when (null (gethash collab--doc-host
                       collab--sync-cursor-timer-table))
    ;; Run with an idle timer to we don’t interrupt scrolling, etc.
    (let ((timer (run-with-idle-timer
                  0.5 nil #'collab--send-cursor-pos-1
                  (current-buffer))))
      (puthash collab--doc-host timer
               collab--sync-cursor-timer-table))))

;;; Global state

(defvar collab--my-host-id nil
  "Host id of our own server.")

(defvar collab--jsonrpc-connection nil
  "The JSONRPC connection to the collab process.")

(defvar collab--doc-id-table (make-hash-table :test #'equal)
  "A has table that maps /HOST/PROJECT/PATH to its (doc-id . host-id), if any.")

(defvar collab--buffer-table (make-hash-table :test #'equal)
  "A has table that maps (doc-id . host-id) to its corresponding buffer.
We also map directory path /HOST/PROJECT/PATH to dired buffers in this table.")

(defvar collab--stashed-state-plist nil
  "Used for stashing local state when major mode changes.")

(defvar collab--dispatcher-timer-table
  (make-hash-table :test #'equal)
  "Hash table mapping (doc-id . host-id) to dispatcher timers.")

(defvar collab--current-message nil
  "If non-nil, collab hub will show this message in the UI.
This should be let-bound when calling ‘collab-hub’, rather than
set globally.")

;;; Local state

(defvar-local collab--doc-host nil
  "(DOC-ID . HOST-ID) for the current buffer.")

(defvar-local collab--accepting-connection nil
  "If non-nil, our collab process is accepting remote connections.")

;; FIXME: Change into error log.
(defvar-local collab--most-recent-error nil
  "Most recent error message.")

(defvar-local collab--inhibit-hooks nil
  "When non-nil, before/after-change hooks don’t run.")

(defvar-local collab--changing-region nil
  "Records information of the region to be changed.
The shape is (beg end content), where CONTENT is the buffer
content between BEG and END.")

(defvar-local collab--pending-ops nil
  "Ops waiting to be sent out. In reverse order.")

(defvar-local collab--group-seq nil
  "Group sequence number.
Group sequence is used to mark undo groups. consecutive ops with
the same group sequence are undone together.")

(defvar-local collab--send-ops-timer nil
  "Timer used to send ops to collab process.
When a user typed enough text, we immediately send them out, but
even if the user only typed a few characters and stopped, we
still want to send them out after a short idle
delay (‘collab-send-ops-delay’).")

(defvar-local collab--send-ops-idle-timer nil
  "Idle timer for sending out ops.")

(defvar-local collab--list-files-cache (make-hash-table :test #'equal)
  "A cache of response of ListFiles.
A hash table; keys are host ids and values are ListFiles reply
objects.")

(defvar-local collab--open-this-doc nil
  "When non-nil, open this doc when we finish refresh.
It should be a list (HOST-ID FILE-DESC FILENAME DIRECTORY-P).
‘collab--insert-host-callback’ will open this doc if it’s in the
listed files.")

(defvar-local collab--hl-ov-1 nil
  "Overlay used for 1st level highlight.")

(defvar-local collab--hl-ov-2 nil
  "Overlay used for 2nd level highlight.")

(defvar-local collab--default-directory nil
  "Default directory for ‘collab-find-file’.
Stored as HOST/TYPE/PATH where TYPE is ‘p’ for project or ‘f’ for file.")

;;; Edit tracking
;;
;; Op := INS | DEL
;; INS := (ins POS STR GROUP-SEQ)
;; DEL := (del ((POS STR)...) GROUP-SEQ)
;; POS := <integer>
;; STR := <string>
;; GROUP-SEQ := <integer>
;;
;; Group sequence is used to mark undo groups. consecutive ops with
;; the same group sequence are undone together.
;;
;; Edit tracking is made of two parts, before-change-function and
;; after-change-function. In before-change-fn, we record the buffer
;; content of the to-be-changed region (so we know what’s deleted if
;; the change is a delete), and in after-change-fn, we create the op.
;; Emacs has more than insertion and deletion: a region could be
;; replaced, which we model as delete + insert. Also a
;; before-change-fn could be paired with multiple after-change-fn in
;; that region, but this doesn’t affect our purpose.
;;
;; Edit tracking is complicated by undo groups and op amalgamation.
;; When you indent a region, the indent function will make multiple
;; edits in different places, and we want to mark all these changes as
;; one undo group, so that they are undone together. Consecutive
;; insertion or deletion should be amalgamated together, to save space
;; and to be undone together.
;;
;; Undo groups are archived by group sequence numbers. Ops with the
;; same group sequence are consider to be in the same group. We
;; increment the group sequence in pre-command-hook, so ops make in
;; the same command are grouped together.
;;
;; Amalgamation is done when adding ops to pending-ops. If the current
;; command only created one op (usually inserting or deleting one
;; character), we try to amalgamate it with the previous op.
;;
;; Originally we use a complicated condition to decide when to send
;; out ops. But now we follow eglot’s example and simply use an idle
;; timer (default to 0.5s) to send out ops. Every op in the same
;; “batch” has the same group seq.

(defvar collab--after-change-hook nil
  "Hook ran in ‘collab--after-change’.")

;; This is for easier testing: we can set this to a mock function for
;; tests.
(defvar-local collab--send-ops-fn #'collab--send-ops
  "Function for sendint ops.")

(defun collab--host-alist ()
  "Return ‘collab--host-alist’ plus outselves.

Don’t use this function before a connection is established, because it
uses ‘collab--my-host-id’."
  (cons `(,collab--my-host-id . ("" ""))
        collab-host-alist))

(defun collab--op-empty-p (op)
  "Return non-nil if OP is an empty ins/del op."
  (eq 0 (length (nth 2 op))))

(defun collab--before-change (beg end)
  "Records the region (BEG . END) to be changed."
  ;; Each before-change can be followed by zero or more after-change.
  ;; So collab--changing-region could be non-nil when we enter this
  ;; function. In that case, just override it.
  (when (not collab--inhibit-hooks)
    (when (not (eq beg end))
      (setq collab--changing-region
            (list beg end (buffer-substring-no-properties beg end))))))

(defun collab--push-to-pending-ops (ops)
  "Push OPS to ‘collab--pending-ops’, amalgamate if possible."
  (when collab--verbose
    (message "Push ops: %s" ops))
  (when (not (eq (car collab--pending-ops) 'tracking-error))
    (let ((amalgamated-op
           (and (eq 1 (length ops))
                (let ((this-op (car ops))
                      (last-pending-op (car collab--pending-ops)))
                  (and last-pending-op
                       (collab--maybe-amalgamate last-pending-op this-op))))))
      (if (null amalgamated-op)
          ;; No amalgamate.
          (setq collab--pending-ops (nconc ops collab--pending-ops))
        ;; Amalgamated.
        (if (collab--op-empty-p amalgamated-op)
            (setq collab--pending-ops (cdr collab--pending-ops))
          (setcar collab--pending-ops amalgamated-op))

        (when collab--verbose
          (message "%s "
                   (string-replace
                    "\n" "\\n"
                    (format "Amalgamate, %s" collab--pending-ops))))))))

(defun collab--after-change (beg end len)
  "Record the changed region (BEG END LEN)."
  (let ((group-seq collab--group-seq)
        ops)
    (catch 'term
      (when collab--inhibit-hooks
        (throw 'term nil))
      (if (eq len 0)
          ;; a) Insert.
          (push (list 'ins beg (buffer-substring-no-properties beg end)
                      group-seq)
                ops)
        ;; Not insert.
        (pcase collab--changing-region
          (`(,rbeg ,rend ,content)
           (if (<= rbeg beg end rend)
               (let ((old (substring
                           content (- beg rbeg) (+ (- beg rbeg) len)))
                     (current (buffer-substring-no-properties beg end)))
                 (if (eq beg end)
                     ;; b) Delete.
                     (push (list 'del beg old group-seq) ops)
                   (if (equal old current)
                       ;; c) Text prop change.
                       nil
                     ;; d) Replace.
                     (if (string-prefix-p old current)
                         ;; If OLD is a prefix of CURRENT, this is
                         ;; actually an insert.
                         (push (list (+ beg (length old))
                                     (substring current (length old))
                                     group-seq)
                               ops)
                       ;; Actual replace.
                       (push (list 'del beg old group-seq) ops)
                       (push (list 'ins beg current group-seq) ops)))))

             (setq collab--pending-ops `(tracking-error (format "Assertion failed: (<= rbeg beg end rend): %s" (rbeg beg end rend))))))
          (_ (setq collab--pending-ops `(tracking-error (format "Unexpected ‘collab--changing-region’ shape: %s" collab--changing-region))))))

      ;; Update state.
      (setq collab--changing-region nil)
      (collab--push-to-pending-ops ops)

      (run-hooks 'collab--after-change-hook))))

(defun collab--after-change-default ()
  "Default send-ops behavior: setup an idle timer to send ops."
  (when collab--send-ops-idle-timer
    (cancel-timer collab--send-ops-idle-timer))
  (let ((buf (current-buffer)))
    (setq collab--send-ops-idle-timer
          (run-with-idle-timer
           collab-send-ops-delay nil
           (lambda ()
             (when (buffer-live-p buf)
               (with-current-buffer buf
                 (unwind-protect
                     (collab--send-ops-now)
                   (setq collab--send-ops-idle-timer nil)))))))))

(defun collab--after-change-hasty ()
  "Send ops in every after-change function."
  (collab--send-ops-now))

(defun collab--send-ops-now ()
  "Run in ‘collab--send-ops-hook’, send ‘collab--pending-ops’."
  (if (eq (car collab--pending-ops) 'tracking-error)
      ;; TODO, send full buffer.
      (when collab--verbose
        (message "Hit a tracking error! Sending full buffer")
        (message "Reason: %s" (cadr collab--pending-ops)))
    (unwind-protect
        (let ((ops (nreverse collab--pending-ops)))
          (when collab--verbose
            (message "Sending ops: %s" ops))
          (setq collab--pending-ops nil)
          (funcall collab--send-ops-fn ops))
      (when (numberp collab--group-seq)
        (cl-incf collab--group-seq)))))

;; https://stackoverflow.com/questions/6590889
;; TODO: Support for overwrite-mode, abbrev-mode, auto-fill.
(defun collab--maybe-amalgamate (op1 op2)
  "Maybe amalgamate OP2 into OP1.
Return the new op if amalgamated, return nil if didn’t amalgamate."
  (pcase (cons op1 op2)
    (`((ins ,pos1 ,str1 ,seq1) . (ins ,pos2 ,str2 ,_))
     ;; Amalgamate if OP2 inserts right after OP1. Don’t amalgamate if
     ;; OP1 is long enough. Use ‘string-width’ so that one CJK char
     ;; counts as two.
     (if (and (< (string-width str1) 20)
              (eq pos2 (+ pos1 (length str1))))
         (list 'ins pos1 (concat str1 str2) seq1)
       nil))

    (`((del ,pos1 ,str1 ,seq1) . (del ,pos2 ,str2 ,_))
     ;; Amalgamate if OP2 deletes right before OP1. Don’t amalgamate
     ;; if OP2 is long enough.
     (if (and (< (string-width str1) 20)
              (eq (+ pos2 (length str2)) pos1))
         (list 'del pos2 (concat str2 str1) seq1)
       nil))
    (`((ins ,pos1 ,str1 ,seq1) . (del ,pos2 ,str2 ,_))
     ;; Amalgamate if OP2 deletes the end of OP1. If the result is an
     ;; empty op, the caller will remove it from pending ops.
     ;; ‘electric-pair-mode’ does this: when you insert "(", it first
     ;; deletes the "(", then inserts "(" and ")" LOL.
     (let ((del-end-pos (+ pos2 (length str2)))
           (ins-end-pos (+ pos1 (length str1))))
       (if (and (<= pos1 pos2) (eq del-end-pos ins-end-pos))
           (list 'ins pos1 (substring str1 0 (- pos2 pos1)) seq1)
         nil)))))

;; FIXME: Add reconnect state.
(defvar collab--mode-line
  `(collab-monitored-mode
    ,(propertize "CONNECTED " 'face
                 '(:inherit (bold success))))
  "Mode-line indicator for ‘collab-monitored-mode’.")

(defun collab--warn-for-unsupported-undo (&rest _)
  "This is bound to undo commands that we don’t support.
To prevent them from being invoked."
  (interactive)
  (message (collab--fairy "Other undo won’t work in this mode, use ‘collab-undo/redo’: (undo → \\[collab-undo], redo → \\[collab-redo])")))

(defvar collab-monitored-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map [remap undo-only] #'collab-undo)
    (define-key map [remap undo-redo] #'collab-redo)
    (define-key map (kbd "C-/") #'collab-undo)
    (define-key map (kbd "C-.") #'collab-redo)
    (define-key map [remap undo] #'collab--warn-for-unsupported-undo)
    ;; Masks.
    (define-key map [remap vundo] #'collab--warn-for-unsupported-undo)
    (define-key map [remap undo-tree-undo]
                #'collab--warn-for-unsupported-undo)
    (define-key map [remap undo-tree-redo]
                #'collab--warn-for-unsupported-undo)
    (define-key map [remap undo-tree-visualize]
                #'collab--warn-for-unsupported-undo)
    ;; Other commands
    ;; FIXME: add collab-save-buffer.
    (define-key map [remap save-buffer] #'collab-save-buffer)
    map)
  "Keymap for ‘collab-monitored-mode’.")

(defvar collab--dispatcher-timer-table)
(define-minor-mode collab-monitored-mode
  "Collaborative monitor mode."
  :global nil
  :keymap collab-monitored-mode-map
  (if collab-monitored-mode
      (progn
        (add-hook 'change-major-mode-hook #'collab--rescue-state 0 t)
        (add-hook 'after-change-major-mode-hook
                  #'collab--maybe-recover 0)
        (add-hook 'before-change-functions
                  #'collab--before-change 0 t)
        (add-hook 'after-change-functions #'collab--after-change 0 t)
        (if collab-hasty-p
            (add-hook 'collab--after-change-hook #'collab--after-change-default 0 t)
          (add-hook 'collab--after-change-hook #'collab--after-change-hasty 0 t))

        (unless (member collab--mode-line mode-line-misc-info)
          (setq-local mode-line-misc-info
                      (cons collab--mode-line mode-line-misc-info)))

        ;; (setq-local buffer-undo-list t)

        (setq-local collab--pending-ops nil)
        (setq-local collab--ops-current-command nil)
        (setq-local collab--group-seq 1))

    ;; Disable.
    (remhash collab--doc-host
             collab--dispatcher-timer-table)

    (remove-hook 'before-change-functions #'collab--before-change t)
    (remove-hook 'after-change-functions #'collab--after-change t)
    (remove-hook 'collab--after-change-hook #'collab--after-change-default)
    (remove-hook 'collab--after-change-hook #'collab--after-change-hasty)

    (remhash collab--doc-host collab--buffer-table)

    (dolist (ov collab--cursor-ov-alist)
      (delete-overlay (cdr ov)))
    (setq-local collab--cursor-ov-alist nil)

    ;; Don’t set doc and host to nil, so that ‘collab-reconnect’ can
    ;; auto-resume.
    ;; (setq-local collab--doc-host nil)

    (setq-local collab--my-site-id nil)))

(defun collab--enable (doc-id host-id my-site-id path)
  "Enable ‘collab-monitored-mode’ in the current buffer.
DOC-ID and HOST-ID are associated with the current buffer.
MY-SITE-ID is the site id of this editor.

PATH should be a string HOST/PROJECT/PATH returned by
‘collab--encode-filename’."
  (setq collab--doc-host (cons doc-id host-id))
  (setq collab--my-site-id my-site-id)
  (setq collab--default-directory path)
  (puthash path collab--doc-host collab--doc-id-table)
  (puthash collab--doc-host
           (current-buffer)
           collab--buffer-table)
  (collab-monitored-mode)
  (let ((pulse-delay 0.1))
    (pulse-momentary-highlight-region
     (point-min) (point-max) 'collab-success-background)))

(defun collab--disable ()
  "Disable ‘collab’ for the current buffer and flash red."
  (let ((pulse-delay 0.1))
    (pulse-momentary-highlight-region
     (point-min) (point-max) 'collab-failure-background))
  (collab-monitored-mode -1))

(defun collab--rescue-state ()
  "Stash local variable elsewhere before major mode changes."
  (setq collab--stashed-state-plist
        (plist-put collab--stashed-state-plist
                   (current-buffer)
                   `( :doc-host ,collab--doc-host
                      :site-id ,collab--my-site-id))))

(defun collab--maybe-recover ()
  "Maybe reenable ‘collab’ after major mode change."
  (when-let* ((state (plist-get collab--stashed-state-plist
                                (current-buffer)))
              (doc-host (plist-get state :doc-host))
              (site-id (plist-get state :site-id)))
    (collab--enable (car doc-host) (cdr doc-host) site-id)))

;;; JSON-RPC

(defun collab--make-file-desc (type &optional id project file)
  "Create a FILE-DESC using TYPE ID PROJECT FILE:

FILE-DESC := (:type \"file\" :id doc-id)
           | (:type \"project\" :id project-id)
           | (:type \"projectFile\" :project project-id :file rel-path)"
  (pcase type
    ("file"
     (cl-assert (stringp id))
     `(:type ,type :id ,id))
    ("project"
     (cl-assert (stringp id))
     `(:type ,type :id ,id))
    ("projectFile"
     (cl-assert (stringp project))
     (cl-assert (stringp file))
     `(:type ,type :project ,project :file ,file))
    (_ (signal 'collab-error `("Invalid type" ,type)))))

(defun collab--file-desc-id (file-desc)
  "Return the doc-id in FILE-DESC if it has one, else return nil.

FILE-DESC := (:type \"file\" :id doc-id)
           | (:type \"project\" :id project-id)
           | (:type \"projectFile\" :project project-id :file rel-path)

Return DOC-ID if FILE-DESC is a :Doc or a :File, or it’s a :Dir
but REL-PATH is “.” Basically, return the doc id if the doc id
actually belongs to FILE-DESC."
  (pcase (plist-get file-desc :type)
    ("file" (plist-get file-desc :id))))

(defun collab--file-desc-parent (file-desc)
  "Return a file desc that’s the parent directory of FILE-DESC."
  (pcase (plist-get file-desc :type)
    ("project" nil)
    ("projectFile"
     (let ((parent (file-name-directory (plist-get file-desc :file))))
       (if (or (equal parent "/") (null parent))
           (collab--make-file-desc "project" (plist-get file-desc :project))
         (collab--make-file-desc
          "projectFile" nil (plist-get file-desc :project) parent))))))

(defun collab--file-desc-name (file-desc)
  "Return the filename of FILE-DESC. Only works for project or projectFile."
  (pcase (plist-get file-desc :type)
    ("project" (plist-get file-desc :id))
    ("projectFile" (file-name-nondirectory (plist-get file-desc :file)))))

;; FIXME: Update or remove.
(defun collab--doc-desc-path (doc-desc)
  "Return the relative path in DOC-DESC."
  (pcase doc-desc
    (`(:File [,_ ,path]) path)
    (`(:Dir [,_ ,path]) path)
    (_ nil)))

;; FIXME: Update or remove.
(defun collab--doc-desc-parent (doc-desc)
  "Return parent doc-desc of DOC-DESC.
If DOC-DESC is a doc, return nil.
If DOC-DESC is at the top-level, return itself."
  (pcase doc-desc
    (`(:File [,doc-id ,path])
     `(:File [,doc-id ,(string-trim-right
                        (or (file-name-directory path) ".")
                        "/")]))
    (`(:Dir [,doc-id ,path])
     `(:Dir [,doc-id ,(string-trim-right
                       (or (file-name-directory path) ".")
                       "/")]))
    (_ nil)))

;;;; Connection

(defun collab--connect-process ()
  "Get existing JSONRPC connection or create one."
  (or (and collab--jsonrpc-connection
           (jsonrpc-running-p collab--jsonrpc-connection)
           collab--jsonrpc-connection)

      (let* ((process (pcase collab-connection-type
                        (`(socket ,port)
                         (open-network-stream
                          "collab" nil "localhost" port))
                        ('pipe
                         (make-process
                          :name "collab"
                          :noquery t
                          :command (list collab-command "run")
                          :connection-type 'pipe)))))

        (let* ((conn (make-instance 'jsonrpc-process-connection
                                    :name "collab"
                                    :process process
                                    :notification-dispatcher
                                    #'collab--dispatch-notification))
               (resp (jsonrpc-request conn 'Initialize nil)))

          (setq collab--my-host-id (plist-get resp :hostId))

          (when collab--jsonrpc-connection
            (jsonrpc-shutdown collab--jsonrpc-connection))
          (setq collab--jsonrpc-connection conn)))))

;;;; Dispatcher

(defun collab--cancel-get-ops-timer ()
  "If there’s a timer for getting remote ops, cancel it.
The purpose of this function is to cancel get ops timer when we
know that we’re gonna send ops (which gets remotes ops too) very
soon."
  (when-let* ((timer (gethash collab--doc-host
                              collab--dispatcher-timer-table)))
    (cancel-timer timer))  )

(defun collab--request-remote-ops (buffer seq)
  "Request for remote ops in BUFFER.
We expect to get all the ops before and including SEQ."
  (let ((received-largest-seq nil))
    (unwind-protect
        (with-current-buffer buffer
          (setq received-largest-seq (collab--send-ops nil)))
      (when (and received-largest-seq
                 (< received-largest-seq seq))
        (with-current-buffer buffer
          (when collab-monitored-mode
            (collab--disable))
          (display-warning 'collab (format "Received less ops than expected, expecting op(%s), received op(%s)" seq received-largest-seq)))))))

(defun collab--dispatch-notification (_conn method params)
  "Dispatch JSONRPC notifications.

CONN is the connection object; METHOD is a symbol naming the
JSONRPC method invoked remotely; and PARAMS is a JSONRPC ‘params’
object.

When we receive a RemoteOpArrived notification, we create a
timer, which will switch to the corresponding buffer and use
SendOp request to fetch remote ops. We use a timer so that a) it
runs after the current command loop ends, and b) multiple
consecutive RemoteOpArried notification only cause the timer to
run once in each command loop.

If we receive a ServerError notification, just display a warning."
  (pcase method
    ('RemoteOpArrived
     (let ((doc (plist-get params :docId))
           (host (plist-get params :hostId))
           (last-seq (plist-get params :lastSeq)))
       (let ((buffer (gethash (cons doc host)
                              collab--buffer-table))
             (timer (gethash (cons doc host)
                             collab--dispatcher-timer-table)))
         (when timer (cancel-timer timer))
         (when (buffer-live-p buffer)
           (setq timer (run-with-timer
                        collab-receive-ops-delay nil
                        #'collab--request-remote-ops
                        buffer last-seq))
           (puthash (cons doc host) timer
                    collab--dispatcher-timer-table)))))
    ('Info
     (let* ((doc-id (plist-get params :docId))
            (host-id (plist-get params :hostId))
            (site-id (plist-get params :senderSiteId))
            (value (plist-get params :value)))
       (pcase (plist-get value :type)
         ("common.pos"
          (let* ((pos (plist-get value :point))
                 (mark (plist-get value :mark))
                 (buf (gethash (cons doc-id host-id)
                               collab--buffer-table)))
            (when (buffer-live-p buf)
              (with-current-buffer buf
                (collab--move-cursor site-id pos mark))))))))
    ('ServerError
     (display-warning
      'collab
      (format "Local host errored, you might want to restart collab process. Cause: %s"
              params)))
    ('SignalingTimesUp
     (run-with-timer
      0.5 nil
      (lambda ()
        (with-current-buffer (collab--hub-buffer)
          (setq collab--accepting-connection nil)
          (collab--refresh)
          (message "Collab process stopped accepting connections")))))
    ('AcceptConnectionErr
     ;; If we use ‘run-with-timer’, this handler might interfer with
     ;; the condition-case handler in ‘collab--render’.
     (run-with-timer
      0.0001 nil
      (lambda ()
        (with-current-buffer (collab--hub-buffer)
          (setq collab--accepting-connection nil)
          (collab--hub-refresh)
          (display-warning
           'collab
           (format "Error accepting remote peer connections: %s" params))))))))

;;;; Requests

(defun collab--list-files-req
    (dir host signaling-server credential &optional callback)
  "Request for files on HOST with CREDENTIAL.

If DIR is non-nil, it should be the FILE-DESC of the directory we want
to list. ‘collab--make-file-desc’ can create it. SIGNALING-SERVER is the
address of the signaling server.

Return a plist (:files [(:docDesc DOC-DESC :fileName FILE-NAME) ...]).

If CALLBACK is non-nil, this request is async and returns nil
immediately, when the response returns, CALLBACK is called with
the response object (not a list of files, but the full plist
response)."
  (let ((conn (collab--connect-process))
        (request-object
         (append `( :hostId ,host
                    :signalingAddr ,signaling-server
                    :credential ,credential)
                 (and dir `(:dir ,dir)))))
    (if callback
        ;; Async request.
        (jsonrpc-async-request
         conn 'ListFiles
         request-object
         :success-fn (lambda (resp)
                       (funcall callback :success resp))
         :error-fn (lambda (resp)
                     (funcall callback :error resp))
         :timeout-fn (lambda ()
                       (funcall callback :timeout resp))
         :timeout collab-connection-timeout)
      ;; Sync request.
      (jsonrpc-request
       conn 'ListFiles
       request-object
       :timeout collab-rpc-timeout))))

(defun collab--share-file-req (filename file-meta content)
  "Share the file with HOST.
FILENAME is filename, FILE-META is a plist-encoded JSON object,
CONTENT is just the content of the file in a string.
Return (:docId DOC-ID :siteId SITE-ID)."
  (let* ((conn (collab--connect-process))
         (resp (jsonrpc-request
                conn 'ShareFile
                `( :filename ,filename
                   :meta ,file-meta
                   :content ,content)
                :timeout collab-rpc-timeout)))
    resp))

(defun collab--declare-project-req (name path)
  "Share a project with NAME and PATH.
PATH must be an absolute path."
  (let* ((conn (collab--connect-process))
         (resp (jsonrpc-request
                conn 'DeclareProjects
                `(:projects [( :name ,name
                               :path ,path)])
                :timeout collab-rpc-timeout)))
    resp))

(defun collab--open-file-req (file-desc host mode)
  "Connect to FILE-DESC on HOST.
MODE can be \"Create\" or \"Open\".
Return (:siteId SITE-ID :content CONTENT)."
  (let* ((conn (collab--connect-process)))
    (message (collab--fairy "Getting the file for ya! Timeout is (checking notes) %s seconds!"
                            (* 3 collab-rpc-timeout)))
    (let ((resp (jsonrpc-request
                 conn 'OpenFile
                 `( :hostId ,host
                    :fileDesc ,file-desc
                    :mode ,mode)
                 :timeout (* 3 collab-rpc-timeout))))
      (message (collab--fairy "Here it is!"))
      resp)))

(defun collab--disconnect-from-file-req (doc host)
  "Disconnect from DOC on HOST."
  (let ((conn (collab--connect-process)))
    (jsonrpc-request conn 'DisconnectFromFile
                     `( :hostId ,host
                        :docId ,doc)
                     :timeout collab-rpc-timeout)))

(defun collab--save-file-req (doc host)
  "Save from DOC on HOST to disk."
  (let ((conn (collab--connect-process)))
    (jsonrpc-request conn 'SaveFile
                     `( :hostId ,host
                        :docId ,doc)
                     :timeout collab-rpc-timeout)))

(defun collab--move-file-req (project-id old-path new-path host)
  "Move file from OLD-PATH to NEW-PATH in PROJECT-ID on HOST."
  (let ((conn (collab--connect-process)))
    (jsonrpc-request conn 'MoveFile
                     `( :hostId ,host
                        :project ,project-id
                        :oldPath ,old-path
                        :newPath ,new-path)
                     :timeout collab-rpc-timeout)))

(defun collab--delete-file-req (doc host)
  "Delete DOC on HOST."
  (let ((conn (collab--connect-process)))
    (jsonrpc-request conn 'DeleteFile
                     `( :hostId ,host
                        :docId ,doc)
                     :timeout collab-rpc-timeout)))

(defun collab--accept-connection-notif (signaling-addr)
  "Accept connections on SIGNALING-ADDR."
  (let ((conn (collab--connect-process)))
    (jsonrpc-notify conn 'AcceptConnection
                    `( :signalingAddr ,signaling-addr
                       :transportType "SCTP"))))

(defun collab--connect-notif (host-id signaling-addr)
  "Connect to HOST-ID on SIGNALING-ADDR."
  (let ((conn (collab--connect-process)))
    (jsonrpc-notify conn 'Connect
                    `( :hostId ,host-id
                       :signalingAddr ,signaling-addr
                       :transportType "SCTP")
                    :timeout collab-rpc-timeout)))

;; FIXME
(defun collab--print-history-req (doc host debug)
  "Print debugging history for DOC on HOST.
If DEBUG non-nil, print debug version."
  (let ((conn (collab--connect-process)))
    (jsonrpc-request conn 'PrintHistory
                     `( :docId ,doc
                        :hostId ,host
                        :debug ,(if debug t :json-false))
                     :timeout collab-rpc-timeout)))

;; FIXME
(defun collab--send-info-req (doc host info)
  "Send INFO to DOC on HOST.
INFO should be a plist JSON object. This request is async."
  (let ((conn (collab--connect-process)))
    (jsonrpc-notify conn 'SendInfo
                    `( :hostId ,host
                       :docId ,doc
                       :info ,info))))

(defsubst collab--encode-op (op)
  "Encode Elisp OP into JSONRPC format."
  (pcase op
    (`(ins ,pos ,str ,group-seq)
     `( :op (:Ins [,(1- pos) ,str])
        :groupSeq ,group-seq))

    (`(del ,pos ,str ,group-seq)
     `( :op (:Del [,(1- pos) ,str])
        :groupSeq ,group-seq))))

(defsubst collab--apply-edit-instruction (instr &optional move-point)
  "Decode EditInstruction INSTR and apply it.
If MOVE-POINT non-nil, move point as the edit would."
  (let ((inhibit-modification-hooks t)
        (collab--inhibit-hooks t)
        (start (point-marker)))
    (save-restriction
      (widen)
      (pcase instr
        (`(:Ins ,edits)
         (mapc (lambda (edit)
                 (let ((pos (aref edit 0))
                       (text (aref edit 1)))
                   (goto-char (1+ pos))
                   (insert text)))
               (reverse edits)))
        (`(:Del ,edits)
         (mapc (lambda (edit)
                 (let ((pos (aref edit 0))
                       (text (aref edit 1)))
                   (delete-region (1+ pos) (+ 1 pos (length text)))))
               (reverse edits)))))
    (unless move-point
      (goto-char start))))

(defun collab--send-ops (ops &optional encoded)
  "Send OPS to the collab process and apply returned remote ops.

If ENCODED is non-nil, OPS should be already in sexp JSON
format (a list of EditorOp, probably the Undo or Redo variant).

Return the largest global seq of all the ops received from the
collab process; return nil if we didn’t get any ops."
  (collab--check-precondition)
  (when collab--verbose
    (message "Sending ops: %s" ops))
  (let ((conn collab--jsonrpc-connection)
        resp)
    (collab--catch-error "can’t send local edits to remote"
      ;; RESP := (:op LEAN-OP :siteId SITE-ID)
      ;; LEAN-OP := (:Ins [POS STR]) | (:Del [[POS STR]])
      (setq resp
            (jsonrpc-request
             conn 'SendOps
             `( :docId ,(car collab--doc-host)
                :hostId ,(cdr collab--doc-host)
                :ops ,(if encoded ops
                        (if ops (vconcat (mapcar #'collab--encode-op ops))
                          [])))
             :timeout collab-rpc-timeout))
      ;; Only ‘seq-map’ can map over vector.
      (let ((remote-ops (plist-get resp :ops))
            (last-seq (plist-get resp :lastSeq))
            last-op)
        (seq-map (lambda (op)
                   (collab--apply-edit-instruction
                    (plist-get op :op))
                   (setq last-op op))
                 remote-ops)
        (pcase last-op
          (`(:op (:Ins ,edits) :siteId ,site-id)
           (when (> (length edits) 0)
             (let* ((last-edit (aref edits (1- (length edits))))
                    (edit-start (aref last-edit 0))
                    (edit-str (aref last-edit 1)))
               (collab--move-cursor
                site-id (+ 1 edit-start (length edit-str))))))
          (`(:op (:Del ,edits) :siteId ,site-id)
           (when (> (length edits) 0)
             (let ((last-edit (aref edits (1- (length edits)))))
               (collab--move-cursor
                site-id (1+ (aref last-edit 0))))))
          ;; If there’s no op to return, collab process returns an
          ;; empty response.
          ('nil nil))
        ;; Return the largest global seq received from collab process.
        last-seq))))

(defun collab--undo (&optional redo)
  "Undo the most recent local edit.
If REDO is non-nil, redo the most recent undo instead."
  (interactive)
  (collab--check-precondition)
  (let ((conn collab--jsonrpc-connection)
        resp)

    (collab--send-ops-now)

    (collab--catch-error "can’t undo/redo"
      (setq resp
            (jsonrpc-request conn 'Undo
                             `( :hostId ,(cdr collab--doc-host)
                                :docId ,(car collab--doc-host)
                                :kind ,(if redo "Redo" "Undo"))
                             :timeout collab-rpc-timeout))
      (if-let* ((instructions (plist-get resp :ops)))
          (if (eq (length instructions) 0)
              (message "No more operations to %s" (if redo "redo" "undo"))
            ;; Only ‘seq-map’ can map over vector. TODO: if the op is
            ;; identify, undo one more step.
            (seq-map (lambda (instr)
                       (collab--apply-edit-instruction instr t))
                     instructions)
            (collab--send-ops
             `[( :op ,(if redo "Redo" "Undo")
                 :groupSeq ,collab--group-seq)]
             t))
        (user-error "No more operations to %s"
                    (if redo "redo" "undo"))))))

(defun collab-undo ()
  "Undo the most recent local edit."
  (interactive)
  (collab--undo))

(defun collab-redo ()
  "Redo the most recent undo operation."
  (interactive)
  (collab--undo t))

(defun collab-save-buffer ()
  "Save the current collab buffer.
Saves the buffer on the owner’s machine."
  (interactive)
  (collab--check-precondition)
  (collab--catch-error "Can’t save the buffer"
    (collab--save-file-req (car collab--doc-host) (cdr collab--doc-host)))
  (message "%s" (collab--fairy "Saved and safe on disk!")))

;;; UI

(defun collab--hub-buffer ()
  "Return the hub buffer."
  (get-buffer-create "*collab hub*"))

(defmacro collab--save-excursion (&rest body)
  "Save position, execute BODY, and restore point.
Point is not restored in the face of non-local exit."
  `(let ((pos (point-marker)))
     ,@body
     (goto-char pos)))

(defface collab-dynamic-highlight
  '((t . (:background "gray90" :extend t)))
  "Face used for the dynamic highlight in the collab buffer.")

(defface collab-host '((t . (:inherit bold)))
  "Face used for the server line in the collab buffer.")

(defface collab-file '((t . (:inherit default)))
  "Face used for files in the collab buffer.")

;;;; Drawing the hub UI

(defun collab--replace-section (prop value content-fn)
  "Replace a section of the buffer with new content.

In the current buffer, search for the section which has text
property PROP with VALUE, and replace that secion with the
content of CONTENT-FN. CONTENT-FN should be a function that
inserts text at point and should leave point at the end of the
new text when it returns. This function re-applies PROP and VALUE
to the new text.

VALUE is compared with ‘equal’.

If there are multiple secions that has PROP and VALUE, only
replace the first such section in the buffer.

If no such section is found, do nothing."
  (let ((inhibit-read-only t)
        match)
    (save-excursion
      (goto-char (point-min))
      (catch 'done
        (while (setq match (text-property-search-forward prop))
          (when (equal (prop-match-value match) value)
            (delete-region (prop-match-beginning match)
                           (prop-match-end match))
            (let ((beg (point)))
              (funcall content-fn)
              (put-text-property beg (point) prop value)
              (throw 'done nil))))))))

(defun collab--insert-file
    (server file-desc filename directoryp &optional display-name)
  "Insert FILE-DESC with FILENAME.

SERVER is its server id. If DISPLAY-NAME non-nil, use that as the
display name.

See ‘collab--file-desc-id’ for the shape of FILE-DESC."
  (let ((beg (point)))
    (insert (propertize (or display-name filename)
                        'face (if directoryp 'dired-directory nil)))
    (add-text-properties
     beg (point) `( collab-file-desc ,file-desc
                    collab-filename ,filename
                    collab-directory-p ,directoryp))))

(defun collab--insert-host-1 (host resp status &optional error)
  "Insert HOST, its STATUS, and its files in RESP.

STATUS can be ‘up’, ‘down’, ‘delayed’, or nil.

RESP can be the response object of ListFiles request or nil.
Insert (empty) if RESP is nil; insert ... if STATUS is not ‘up’.

If ERROR (string) is non-nil, also insert the error. ERROR
shouldn’t end with a newline."
  (let* ((beg (point))
         (files (seq-map #'identity (plist-get resp :files)))
         (projects (seq-filter (lambda (file)
                                 (equal (plist-get (plist-get file :file) :type) "project"))
                               files))
         (docs (seq-filter (lambda (file)
                             (equal (plist-get (plist-get file :file) :type) "file"))
                           files)))
    ;; 1. Insert host line.
    (insert (propertize host
                        'face 'collab-host
                        'collab-host-line t))
    ;; 1.1 Insert status.
    (unless (equal host collab--my-host-id)
      (insert (pcase status
                ('nil "")
                ('delayed (propertize " CONNECTING" 'face 'shadow))
                ('up (propertize " UP" 'face 'success))
                ('down (propertize " DOWN" 'face 'error)))))
    (insert (propertize "\n" 'line-spacing 0.4))
    ;; 2. Insert files.
    (pcase status
      ('up (if (null files)
               (insert (propertize "(empty)\n" 'face 'shadow))
             (cl-loop for collection in (list projects docs)
                      for title in (list "---projects---" "---docs---")
                      do (progn
                           (when collection
                             (insert (propertize title 'face 'shadow) "\n"))
                           (dolist (entry collection)
                             (let* ((file-desc (plist-get entry :file))
                                    (name (plist-get entry :filename))
                                    (directoryp (not (eq (plist-get entry :isDirectory)
                                                         :json-false))))
                               (collab--insert-file host file-desc name directoryp)
                               (insert "\n")))))))
      ;; Delayed. Getting files async.
      (_ (insert (propertize "...\n" 'face 'shadow))))
    ;; 3. Insert error.
    (when error
      (insert (propertize (string-trim error) 'face 'shadow) "\n"))
    ;; 4. Mark host section.
    (add-text-properties beg (point)
                         `(collab-host-id ,host))))

(defun collab--insert-host-callback (host status resp)
  "The callback function for inserting HOST and its files.

STATUS should be one of ‘:success’, ‘:error’, or ‘:timeout’. For
‘:success’, RESP is the plist response object of ListFiles. For
‘:error’, RESP is an error object that contains ‘code’,
‘message’, ‘data’. For ‘:timeout’, resp is nil.

This function finds the section for the host in the CURRENT
BUFFER (make sure it’s the hub buffer), and populates the file
list."
  (pcase status
    (:success
     (puthash host resp collab--list-files-cache)
     ;; Turn vector into list.
     ;; Update the hub buffer.
     (collab--replace-section
      'collab-host-id host
      (lambda ()
        (collab--insert-host-1 host resp 'up)))
     ;; Remove fairy chatter about having no docs.
     (when resp
       (collab--replace-section 'collab-fairy-chatter t #'ignore))
     ;; Open the doc we were trying to open before fetching files.
     (when (and collab--open-this-doc
                (equal host (car collab--open-this-doc)))
       (pcase-let ((`(,_ ,file-desc ,filename ,directory-p)
                    collab--open-this-doc))
         (setq collab--open-this-doc nil)
         (collab--open-thing file-desc filename directory-p host))))
    (:error
     (puthash host nil collab--list-files-cache)
     (collab--replace-section
      'collab-host-id host
      (lambda ()
        (collab--insert-host-1
         host nil 'down
         (or (plist-get resp :message)
             "Error connecting to the host")))))
    (:timeout
     (puthash host nil collab--list-files-cache)
     (collab--replace-section
      'collab-host-id host
      (lambda ()
        (collab--insert-host-1
         host nil 'down
         (format "Timed out connecting to the host after %ss"
                 collab-connection-timeout)))))))

(defun collab--insert-host (host signaling-server credential)
  "Insert HOST on SIGNALING-SERVER and its files.

Server has CREDENTIAL. SIGNALING-SERVER is the address of the
signaling server. Return t if the server has some file to list.

Return t if inserted a host section into the buffer."
  (if (equal host collab--my-host-id)
      ;; Get local host files synchronously.
      (let* ((resp (collab--list-files-req
                    nil host signaling-server credential)))
        ;; Don’t insert local host if there’s no files on it.
        (if (eq (length (plist-get resp :files)) 0)
            nil
          (collab--insert-host-1 host resp 'up)
          t))
    ;; Get remote host files asynchronously.
    (collab--insert-host-1 host nil 'delayed)
    (let ((collab-hub-buf (current-buffer)))
      (collab--list-files-req
       nil host signaling-server credential
       (lambda (status resp)
         (when (buffer-live-p collab-hub-buf)
           (with-current-buffer collab-hub-buf
             (collab--insert-host-callback host status resp))))))
    t))

(defun collab--prop-section (prop)
  "Return the (BEG . END) of the range that has PROP."
  (save-excursion
    (let ((forward-prop (text-property-search-forward prop))
          (backward-prop (text-property-search-backward prop)))
      (cons (prop-match-beginning backward-prop)
            (prop-match-end forward-prop)))))

(defvar collab-hub-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "RET") #'collab--open-thing)
    (define-key map (kbd "x") #'collab--delete-doc)
    (define-key map (kbd "k") #'collab--disconnect-from-doc)
    (define-key map (kbd "l") #'collab-share-link)
    (define-key map (kbd "g") #'collab--refresh)
    (define-key map (kbd "f") #'collab-find-file)
    (define-key map (kbd "A") #'collab--accept-connection)
    (define-key map (kbd "C") #'collab-connect)
    (define-key map (kbd "+") #'collab-share)

    (define-key map (kbd "n") #'next-line)
    (define-key map (kbd "p") #'previous-line)
    map)
  "Keymap for ‘collab-hub-mode’.")

(define-derived-mode collab-hub-mode special-mode "Collab"
  "Collaboration mode."
  (setq-local line-spacing 0.2
              eldoc-idle-delay 0.8)
  (add-hook 'eldoc-documentation-functions #'collab--eldoc 0 t)
  (add-hook 'post-command-hook #'collab--dynamic-highlight)
  (eldoc-mode))

(defun collab--render (&optional use-cache)
  "Render collab mode buffer.
Also insert ‘collab--current-message’ if it’s non-nil.
If USE-CACHE is t, don’t refetch file list, use the cached file list."
  (let ((connection-up nil))
    ;; 1. Insert headers.
    (insert "Connection: ")
    (condition-case err
        (progn
          (collab--connect-process)
          (insert (propertize "UP" 'face 'success) "\n")
          (setq connection-up t))
      (error
       (insert (propertize "DOWN" 'face 'error) "\n")
       (setq collab--most-recent-error
             (format "Cannot connect to collab process: %s"
                     (error-message-string err)))))
    (insert (format "Connection type: %s\n" collab-connection-type))
    (insert "Accepting remote peer connections: "
            (if (and connection-up collab--accepting-connection)
                (propertize "YES" 'face 'success)
              (propertize "NO" 'face 'shadow))
            "\n")
    (when (and connection-up collab--accepting-connection)
      (insert (format "Your address: %s/%s\n"
                      collab-default-signaling-server
                      (or collab--my-host-id "???"))))
    (insert "\n\n")

    ;; 2. Insert notice message, if any.
    (when collab--current-message
      (delete-char -1)
      (let ((beg (point)))
        (insert "\n" (string-trim collab--current-message) "\n\n")
        (font-lock-append-text-property beg (point) 'face 'diff-added))
      (insert "\n"))

    ;; 3. Insert each host and files.
    (let ((inserted-any-host nil))
      (dolist (entry (collab--host-alist))
        (let* ((host-id (car entry))
               (signaling-server (nth 0 (cdr entry)))
               (credential (nth 1 (cdr entry)))
               (cached-resp (gethash host-id collab--list-files-cache))
               (inserted-host nil))
          (cond
           ((and use-cache cached-resp)
            (collab--insert-host-1 host-id cached-resp 'up)
            (setq inserted-host t))
           ((not connection-up)
            (unless (equal host-id collab--my-host-id)
              (collab--insert-host-1 host-id nil nil)
              (setq inserted-host t)))
           (connection-up
            (setq inserted-host
                  (collab--insert-host
                   host-id signaling-server credential))))
          (when inserted-host
            (insert "\n")
            (setq inserted-any-host t))))
      (when inserted-any-host
        (insert "\n")))

    ;; 4. Fairy chatter.
    (let ((has-some-doc
           (save-excursion
             (goto-char (point-min))
             (text-property-search-forward 'collab-file-desc)))
          (fairy-chatter
           (propertize
            (concat
             (collab--fairy "No shared docs, not here, not now.
Let’s create one, here’s how!\n\n\n"))
            'collab-fairy-chatter t)))
      (when (not has-some-doc)
        (insert fairy-chatter)))

    ;; 5. Footer.
    (insert (substitute-command-keys
             "PRESS \\[collab--refresh] TO REFRESH
PRESS \\[collab-share] TO SHARE A FILE/DIR
PRESS \\[collab-connect] TO CONNECT TO A REMOTE DOC
PRESS \\[collab--accept-connection] TO ACCEPT REMOTE CONNECTIONS (for 180s)\n"))
    (insert "\n\n")

    ;; 6. Error.
    (when collab--most-recent-error
      (insert
       (propertize (concat "Most recent error:\n\n"
                           collab--most-recent-error)
                   'face 'shadow))
      (setq collab--most-recent-error nil))
    (insert "\n")))

;;;; Dynamic highlight

(defun collab--dynamic-highlight ()
  "Highlight host and file at point."
  (let ((ov (or collab--hl-ov-1
                (let ((ov (make-overlay 1 1)))
                  (overlay-put ov 'face 'collab-dynamic-highlight)
                  (setq collab--hl-ov-1 ov)))))
    (cond
     ((get-text-property (point) 'collab-doc-desc)
      (let ((range (collab--prop-section 'collab-doc-desc)))
        (move-overlay ov (car range) (cdr range)))
      ;; (setq-local cursor-type nil)
      )
     ((get-text-property (point) 'collab-host-line)
      (let ((range (collab--prop-section 'collab-host-id)))
        (move-overlay ov (car range) (cdr range)))
      ;; (setq-local cursor-type nil)
      )
     (t
      (when ov
        (move-overlay ov 1 1))
      ;; (kill-local-variable 'cursor-type)
      ))))

;;;; Eldoc

;;; Dired

(defun collab--eldoc (callback)
  "Displays help information at point.
Used for ‘eldoc-documentation-functions’, calls CALLBACK
immediately."
  (let ((doc-desc (get-text-property (point) 'collab-doc-desc))
        (host-id (get-text-property (point) 'collab-host-id)))
    (cond
     (doc-desc
      (funcall
       callback
       (substitute-command-keys
        "\\[collab--open-thing] → Open Doc/Project  \\[collab--delete-doc] → Delete  \\[collab--disconnect-from-doc] → Disconnect  \\[collab-share-link] → Copy share link")))
     (host-id
      (funcall callback (substitute-command-keys
                         "\\[collab-share] → Share File/directory")))
     (t
      (funcall callback (substitute-command-keys
                         "\\[collab--refresh] → Refresh"))))))

(defvar collab-dired-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "RET") #'collab--open-thing)
    (define-key map (kbd "x") #'collab--delete-doc)
    (define-key map (kbd "k") #'collab--disconnect-from-doc)
    (define-key map (kbd "g") #'collab--dired-refresh)

    (define-key map (kbd "n") #'next-line)
    (define-key map (kbd "p") #'previous-line)
    map)
  "Keymap for ‘collab-dired-mode’.")

(define-derived-mode collab-dired-mode special-mode "Collab Dired"
  "Mode showing a shared directory.")

(defun collab--dired-refresh ()
  "Refresh the current ‘collab-dired-mode’ buffer."
  (interactive)
  (let ((inhibit-read-only t)
        (line (line-number-at-pos)))
    (erase-buffer)
    (collab--dired-render)
    (goto-char (point-min))
    (forward-line (1- line))))

(defun collab--dired-render ()
  "Draw the current ‘collab-dired-mode’ buffer from scratch."
  (when (and (derived-mode-p 'collab-dired-mode) collab--doc-host)
    (collab--catch-error (format "can’t connect to %s"
                                 (car collab--doc-host))
      (let* ((file-desc (car collab--doc-host))
             (host-id (cdr collab--doc-host))
             (host-data (alist-get host-id (collab--host-alist)
                                   nil nil #'equal))
             (projectp (equal (plist-get file-desc :type) "project"))
             (file-list-resp (collab--list-files-req
                              file-desc host-id (nth 0 host-data) (nth 1 host-data)))
             (file-list (seq-map #'identity
                                 (plist-get file-list-resp :files)))
             (parent-file-desc (collab--file-desc-parent file-desc)))
        (insert (propertize (collab--encode-filename file-desc host-id) 'face 'dired-header)
                ":\n")
        (insert ".\n")
        (if projectp
            (insert "..")
          (collab--insert-file
           host-id parent-file-desc (collab--file-desc-name parent-file-desc) t ".."))
        (insert "\n")
        (if (null file-list)
            (insert (collab--fairy "There’s no files here!\n"))
          (dolist (entry file-list)
            (let* ((file-desc (plist-get entry :file))
                   (filename (plist-get entry :filename))
                   (directoryp (not (eq (plist-get entry :isDirectory) :json-false))))
              (collab--insert-file host-id file-desc filename directoryp)
              (insert "\n"))))
        (put-text-property (point-min) (point-max)
                           'collab-host-id host-id)))))


;;; Interactive commands

(defun collab-shutdown ()
  "Shutdown the connection to the collab process."
  (interactive)
  (when collab--jsonrpc-connection
    (jsonrpc-shutdown collab--jsonrpc-connection)
    (setq collab--jsonrpc-connection nil)))

(defun collab--hub-refresh (&optional use-cache)
  "Refresh collab hub buffer.
If USE-CACHE is t, don’t fetch for file list and use cached file
list."
  (interactive)
  (let ((inhibit-read-only t)
        (line (line-number-at-pos)))
    (erase-buffer)
    (collab--render use-cache)
    (goto-char (point-min))
    (forward-line (1- line))))

(defun collab--refresh ()
  "Refresh current buffer, works in both hub and dired mode."
  (interactive)
  (cond ((derived-mode-p 'collab-hub-mode)
         (collab--hub-refresh))
        ((derived-mode-p 'collab-dired-mode)
         (collab--dired-refresh))))

;;; Find-file

(defun collab--parse-filename (filename)
  "Parse FILENAME into (HOST-ID . FILE-DESC).

Return nil if parse failed."
  (let ((segments (string-split filename "/" t (rx (+ whitespace)))))
    (when (>= (length segments) 2)
      (let ((host-id (pop segments))
            (project (pop segments)))
        (if (equal project "_id")
            (let ((id (pop segments)))
              (when id
                (cons host-id (collab--make-file-desc "file" id))))
          (if segments
              (cons host-id
                    (collab--make-file-desc
                     "projectFile" nil project (string-join segments "/")))
            (cons host-id (collab--make-file-desc "project" project))))))))

(defun collab--encode-filename (file-desc host-id)
  "Encode HOST and FILE-DESC into a path.

Returned path doesn’t have trailing slash even if it’s a directory."
  (pcase (plist-get file-desc :type)
    ("file" (format "/%s/_id" host-id))
    ("project" (format "/%s/%s" host-id (plist-get file-desc :id)))
    ("projectFile"
     (let* ((project (plist-get file-desc :project))
            (file-path (plist-get file-desc :file))
            (parent (file-name-directory file-path)))
       (if parent
           (format "/%s/%s/%s" host-id project
                   (string-trim-right parent "/"))
         (format "/%s/%s" host-id project))))))

(defun collab--complete-filename (filename _pred flag)
  "Complete FILENAME.
PRED and FLAG see manual."
  (let* ((append-slash (lambda (str) (concat str "/")))
         (dir-p (string-match-p (rx "/" eos) filename))
         (start-with-slash-p (string-match-p (rx bos "/") filename))
         (segments (string-split filename "/" t (rx (+ whitespace))))
         (next (pcase (length segments)
                 (0 (seq-map append-slash (collab--get-available-hosts)))
                 (_ (condition-case nil
                        (let* ((host (car segments))
                               (host-data (alist-get host (collab--host-alist)
                                                     nil nil #'equal))
                               (signaling (nth 0 host-data))
                               (credential (nth 1 host-data))
                               (file-desc (pcase (length segments)
                                            (1 nil)
                                            (2 (collab--make-file-desc
                                                "project" (nth 1 segments)))
                                            (_ (collab--make-file-desc
                                                "projectFile" nil
                                                (nth 1 segments)
                                                (string-join
                                                 (seq-subseq segments 2 (if dir-p nil -1))
                                                 "/")))))
                               (resp (collab--list-files-req
                                      file-desc host signaling credential))
                               (files (plist-get resp :files)))
                          (seq-map (lambda (file)
                                     (let ((name (plist-get file :filename))
                                           (dir-p (not (eq (plist-get file :isDirectory)
                                                           :json-false))))
                                       (concat name (if dir-p "/" ""))))
                                   files))
                      (error nil))))))

    (pcase flag
      ('nil (if (null next)
                t
              filename))
      ('t next)
      ('lambda (if (null next)
                   t
                 nil))
      (`(boundaries . ,suffix) ; Mimic what ‘read-file-name-internal’ does.
       (cons 'boundaries (cons (1+ (or (cl-position ?/ filename :from-end t) -1))
                               (or (cl-position ?/ suffix) 0))))
      ('metadata '(metadata (category . file)))
      (_ nil))))

(defun collab--get-available-hosts ()
  "Return a list of available host IDs."
  (mapcar #'car (collab--host-alist)))

(defun collab--open-thing (&optional file-desc filename directory-p host-id)
  "Open the file at point.
There should be four text properties at point:

- ‘collab-file-desc’
- ‘collab-filename’
- ‘collab-directory-p’
- ‘collab-host-id’

If FILE-DESC, FILENAME, DIRECTORY-P, HOST-ID non-nil, use them instead."
  (interactive)
  ;; See ‘collab--file-desc-id’ for the shape of FILE-DESC.
  (let* ((file-desc (or file-desc
                        (get-text-property (point) 'collab-file-desc)))
         (filename (or filename (get-text-property (point) 'collab-filename)))
         (directory-p (or directory-p (get-text-property (point) 'collab-directory-p)))
         (host-id (or host-id (get-text-property (point) 'collab-host-id)))
         (path (collab--encode-filename file-desc host-id))
         (buf (gethash (gethash path collab--doc-id-table) collab--buffer-table)))
    (cond
     ((not (and file-desc filename host-id)) nil)
     ((buffer-live-p buf)
      (display-buffer buf))
     ;; Open a directory or project.
     (directory-p
      (let* ((path (collab--encode-filename file-desc host-id))
             (buf (get-buffer-create (format "%s<collab>" path))))
        (select-window (display-buffer buf))
        (collab-dired-mode)
        (puthash path buf collab--buffer-table)
        (setq collab--doc-host (cons file-desc host-id)
              collab--default-directory path)
        (collab--dired-refresh)))
     ;; Open a file.
     (t
      (collab--catch-error (format "can’t connect to %s" file-desc)
        (let* ((resp (collab--open-file-req file-desc host-id "open"))
               (site-id (plist-get resp :siteId))
               (content (plist-get resp :content))
               (doc-id (plist-get resp :docId))
               (filename (plist-get resp :filename))
               (meta (plist-get resp :fileMeta))
               (suggested-major-mode (plist-get meta :emacs.majorMode))
               (inhibit-read-only t))
          (select-window
           (display-buffer
            (generate-new-buffer (format "%s<collab>(%s)" filename doc-id))))
          (collab-monitored-mode -1)
          (erase-buffer)
          (insert content)
          (goto-char (point-min))
          (if (and (stringp suggested-major-mode)
                   (functionp (intern-soft suggested-major-mode)))
              (funcall (intern-soft suggested-major-mode))
            (let ((buffer-file-name filename))
              (set-auto-mode)))
          (collab--enable doc-id host-id site-id path)))))))

(defun collab--disconnect-from-doc ()
  "Disconnect from the file at point."
  (interactive)
  (let* ((file-desc (get-text-property (point) 'collab-file-desc))
         (host-id (get-text-property (point) 'collab-host-id))
         (doc-id (collab--file-desc-id file-desc)))
    (when doc-id
      (collab--catch-error
          (format "can’t disconnect from Doc(%s)" doc-id)
        (collab--disconnect-from-file-req doc-id host-id)))
    (let ((buf (when doc-id
                 (gethash (cons doc-id host-id)
                          collab--buffer-table)))
          (inhibit-read-only t))
      (when buf
        (with-current-buffer buf
          (collab--disable)))
      (puthash (cons doc-id host-id) nil
               collab--buffer-table)
      (save-excursion
        (end-of-line)
        (when (looking-back " •" 2)
          (delete-char -2))))))

;; FIXME
(defun collab--delete-doc ()
  "Delete the file at point."
  (interactive)
  (when-let* ((doc-id (collab--file-desc-id
                       (get-text-property (point) 'collab-file-desc)))
              (host-id (get-text-property (point) 'collab-host-id)))
    (collab--catch-error (format "can’t delete Doc(%s)" doc-id)
      (collab--delete-file-req doc-id host-id))
    (collab--refresh)))

(defun collab--accept-connection ()
  "Start accepting connections."
  (interactive)
  (let ((host-id (nth 0 collab-local-host-config))
        (signaling-addr (nth 1 collab-local-host-config)))
    (collab--catch-error "can’t accept connection "
      (collab--accept-connection-notif signaling-addr))
    (setq-local collab--accepting-connection t)
    (collab--hub-refresh)))

;;;###autoload
(defun collab-find-file ()
  "Find and open a collab file with auto-completion.
Uses ‘collab--default-directory’ as initial input."
  (interactive)
  (let ((path (completing-read
               "Find collab file: "
               #'collab--complete-filename
               nil nil (or collab--default-directory "/"))))
    (if (not (and path (not (string-empty-p path))))
        (user-error "Path is empty")
      (let ((parsed (collab--parse-filename path)))
        (if (not parsed)
            (message "Invalid path format: %s" path)
          (collab--open-thing (car parsed)
                              (file-name-nondirectory path)
                              (cdr parsed)))))))

;;;###autoload
(defun collab-hub ()
  "Pop up the collab hub interface."
  (interactive)
  (switch-to-buffer (collab--hub-buffer))
  (collab-hub-mode)
  (collab--hub-refresh))

(defun collab--notify-newly-shared-doc (file-desc)
  "In collab hub, insert a notice of the newly shared doc or project (FILE-DESC)."
  (with-current-buffer (collab--hub-buffer)
    (collab--accept-connection)
    (let* ((id (plist-get file-desc :id))
           (type (pcase (plist-get file-desc :type)
                   ("project" "p")
                   ("file" "f")))
           (link (format "%s/%s/%s/%s"
                         collab-default-signaling-server
                         (or collab--my-host-id "???")
                         type
                         id))
           (collab--current-message
            (collab--fairy "The file is shared and here’s the link
Your friends can connect with just a click!
LINK: %s" (propertize link 'face 'link))))
      (collab--hub-refresh))))

;;;###autoload
(defun collab-share-buffer (filename)
  "Share the current buffer with FILENAME.
When called interactively, prompt for the host."
  (interactive (list (read-string
                      "File name: "
                      (or (and buffer-file-name
                               (file-name-nondirectory buffer-file-name))
                          (buffer-name)))))
  (collab--catch-error "can’t share the current buffer"
    ;; For the moment, always share to local host.
    (let* ((host collab--my-host-id)
           (resp (collab--share-file-req
                  filename `( :common.hostEditor "emacs"
                              :emacs.majorMode
                              ,(symbol-name major-mode))
                  (buffer-substring-no-properties
                   (point-min) (point-max))))
           (doc-id (plist-get resp :docId))
           (site-id (plist-get resp :siteId)))
      (collab--enable doc-id host site-id)
      (display-buffer (collab--hub-buffer)
                      '(() . ((inhibit-same-window . t))))
      (collab--notify-newly-shared-doc `(:type "file" :id ,doc-id)))))

;;;###autoload
(defun collab-share (file filename)
  "Share FILE with FILENAME.
FILE can be either a file or a directory."
  (interactive (let* ((path (read-file-name "Share: "))
                      (filename (file-name-nondirectory
                                 (string-trim-right path "/"))))
                 (list path filename)))
  (if (or (when-let* ((target (file-symlink-p file)))
            (file-regular-p target))
          (file-regular-p file))
      (progn
        (find-file file)
        (collab-share-buffer filename))
    (collab--share-project file filename)))

(defun collab--share-project (path project-name)
  "Share project at PATH to collab process under PROJECT-NAME."
  (collab--catch-error "can’t share project"
    (collab--declare-project-req project-name path)
    (collab--notify-newly-shared-doc `(:type "project" :id ,project-name))))

(defalias 'collab-reconnect 'collab-resume)
(defun collab-resume (host doc-id)
  "Reconnect current buffer to a remote document HOST.

Reconnecting will override the current content of the buffer.

The document to reconnect to is determined by DOC-ID. But if
called interactively, prompt the user to select a document (by
its name rather than doc id) to connect."
  (interactive (list (cdr collab--doc-host)
                     (car collab--doc-host)))
  (collab--catch-error (format "can’t reconnect to Doc(%s)" doc-id)
    (let* ((info (alist-get host (collab--host-alist)
                            nil nil #'equal))
           (orig-point (point))
           (file-desc `(:type "file" :id ,doc-id)))
      ;; Replace buffer content with document content.
      (let* ((resp (collab--open-file-req file-desc host "open"))
             (content (plist-get resp :content))
             (site-id (plist-get resp :siteId))
             (inhibit-read-only t))

        (when (buffer-modified-p)
          (when (y-or-n-p "Save buffer before replacing it with remote content?")
            (save-buffer)))

        ;; Same as in ‘collab--open-thing’.
        (collab-monitored-mode -1)
        (erase-buffer)
        (insert content)
        (goto-char orig-point)
        (set-auto-mode)
        (collab--enable doc-id host site-id)))))

(defun collab-disconnect-buffer ()
  "Disconnect current buffer, returning it to a regular buffer."
  (interactive)
  (collab--check-precondition)
  (let ((doc-id (car collab--doc-host))
        (host-id (cdr collab--doc-host)))
    (collab--catch-error
        (format "can’t disconnect from Doc(%s)" doc-id)
      (collab--disconnect-from-file-req doc-id host-id)))
  (collab--disable)
  (message "Disconnected"))

(defun collab-share-link ()
  "Copy the share link of current doc to clipboard."
  (interactive)
  (let ((file-desc
         (if (derived-mode-p 'collab-hub-mode)
             (get-text-property (point) 'collab-file-desc)
           `(:type "file" :id ,(car collab--doc-host)))))
    (if (not file-desc)
        (message (collab--fairy "Uhmmm, I can’t find the doc id of this doc..."))
      (let* ((type (pcase (plist-get file-desc :type)
                     ("project" "p")
                     ("file" "f")))
             (link (format "%s/%s/%s/%s"
                           collab-default-signaling-server
                           (or collab--my-host-id "???")
                           type
                           (plist-get file-desc :id))))
        (kill-new link)
        (message (collab--fairy "Copied link to clipboard! (%s)" link))))))

;;;###autoload
(defun collab-connect (share-link)
  "Connect to the doc at SHARE-LINK.
SHARE-LINK should be in the form of signaling-server/host-id/#/doc-id."
  (interactive (list (read-string "Share link: ")))
  ;; signaling-server/host-id/project/path
  ;; 0                1       2       3
  (let* ((beheaded-link (string-replace "wss://" "" share-link))
         (segments (string-split beheaded-link "/" t))
         (dir-p (or (string-match-p (rx "/" eos) share-link)
                    (<= (length segments) 3)))
         (signaling-addr (concat "wss://" (nth 0 segments)))
         (host-id (nth 1 segments))
         (project (nth 2 segments))
         (path (if (> (length segments) 3)
                   (string-join (seq-subseq segments 3) "/")
                 nil)))

    (unless (alist-get host-id collab-host-alist nil nil #'equal)
      (push (cons host-id (list signaling-addr ""))
            collab-host-alist))

    (if (equal host-id collab--my-host-id)
        ;; TODO fairy.
        (message "You shouldn’t need to connect to a local doc")
      (switch-to-buffer (collab--hub-buffer))
      (collab-hub-mode)
      (when project
        (setq collab--open-this-doc
              (list host-id
                    (if path
                        `(:type "projectFile" :project ,project :file ,path)
                      `(:type "project" :id ,project))
                    (if path (file-name-nondirectory path) project)
                    dir-p)))
      (collab--hub-refresh))))

(defun collab--print-history (&optional debug)
  "Print debugging history for the current buffer.
If called with an interactive argument (DEBUG), print more
detailed history."
  (interactive "p")
  (collab--check-precondition)
  (let ((doc-id (car collab--doc-host))
        (host-id (cdr collab--doc-host))
        (debug (eq debug 4)))
    (collab--catch-error
        (format "can’t print history of Doc(%s)" doc-id)
      (collab--send-ops-now)
      (let ((text (collab--print-history-req doc-id host-id debug))
            undo-tip)
        (pop-to-buffer (format "*collab history for %s*"
                               (buffer-name)))
        (erase-buffer)
        (insert text)
        (goto-char (point-min))
        (re-search-forward
         (rx bol "UNDO TIP: " (group (+ anychar)) eol))
        (setq undo-tip (match-string 1))
        (unless (equal undo-tip "EMPTY")
          (goto-char (point-min))
          (search-forward "UNDO QUEUE")
          (re-search-forward
           (rx-to-string `(group bol ,undo-tip)))
          (end-of-line)
          (insert (propertize "  <--- UNDO TIP" 'face 'error)))))))

;;;###autoload
(defun collab ()
  "The main entry point of ‘collab-mode’."
  (interactive)
  (unless (and collab-display-name collab-local-host-config)
    (collab-initial-setup))
  (let ((resp (read-multiple-choice
               (collab--fairy "Heya! It’s nice to see you. What do you want me to do?\n")
               '((?s "share this buffer" "Share this buffer")
                 (?S "share a file/directory" "Share a file or directory")
                 (?r "reconnect to doc" "Reconnect to a document")
                 (?k "kill (disconnect)" "Disconnect and stop collaboration")
                 (?l "share link" "Copy the share link")
                 (?h "open hub" "Open collab hub")))))
    (pcase (car resp)
      (?s (when (or (null collab-monitored-mode)
                    (y-or-n-p (collab--fairy "Buffer in collab-mode, already quite set. Share it as new doc, confirm without regret? ")))
            (call-interactively #'collab-share-buffer)))
      (?S (call-interactively #'collab-share))
      (?r (call-interactively #'collab-resume))
      (?k (collab-disconnect-buffer))
      (? (collab-share-link))
      (?h (collab-hub)))))

;;; Setup

(defun collab-initial-setup ()
  "Initial setup wizard. Set display name, download binary, etc."
  (let ((display-name (read-string (collab--fairy "Heya! I’m dolly dolly dolly, the collab-mode fairy. Sweet human, tell me, what name do you carry? -- ") user-full-name))
        (default-signal-server "wss://signal.collab-mode.org"))
    (customize-set-variable 'collab-display-name display-name)
    (customize-save-customized)

    ;; (when (and (not (executable-find "collab-mode"))
    ;;            (y-or-n-p (collab--fairy "No binary here, a tiny regret. Shall we fetch from the internet? ")))
    ;;   (let ((choice (car (read-multiple-choice
    ;;                       (collab--fairy "Which one suits you the best? ")
    ;;                       '((?l "linux_x64" "Linux x86_64")
    ;;                         (?m "mac_x64" "Mac x86_64")
    ;;                         (?a "mac_arm" "Mac ARM"))))))
    ;;     (ignore choice)
    ;;     (message "TODO download binary")))
    ))

(provide 'collab-mode)

;;; collab.el ends here
