;;; collab-mode.el --- Collaborative editing mode  -*- lexical-binding: t; -*-

;; Author: Yuan Fu <casouri@gmail.com>

;;; This file is NOT part of GNU Emacs

;;; Commentary:

;;; Dev notes:

;; FILE-DESC is how we address files in collab-mode. It has the following shape:
;;
;; FILE-DESC := (:project PROJECT-NAME :file FILE-PATH)
;;
;; PROJECT-NAME is the name of the project, there’re two reserved
;; projects, “_buffers” for buffers, and “_files” for files not in a
;; project. For files in a project, FILE-PATH is the relative path of
;; that file. If FILE-PATH is empty string, the FILE-DESC represents
;; the project itself.

;;; Code:

(require 'jsonrpc)
(require 'text-property-search)
(require 'rmc)
(require 'icons)
(require 'pulse)
(require 'rainbow-mode) ; For ‘rainbow-x-color-luminance’.

;;; Custom options

(defgroup collab
  ()
  "Collaboration mode."
  :group 'editting)

(defcustom collab-display-name nil
  "The display name of the user."
  :type 'string)

(defcustom collab-accept-connection-on-startup t
  "Whether to start accepting remote connection on start-up."
  :type 'boolean)

;; Generated with seaborn.color_palette("colorblind").as_hex()
(defcustom collab-cursor-colors
  '( "#0173b2" "#de8f05" "#029e73" "#d55e00" "#cc78bc" "#ca9161"
     "#fbafe4" "#949494" "#ece133" "#56b4e9")
  "Cursor color ring."
  :type '(list string))

(defcustom collab-send-ops-delay 0.5
  "Delay for sendin ops to server.

After user typed something and stoped, Emacs waits for this much time to
send the pending ops. If user keeps typing, Emacs waits for the user to
stop typing."
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

(defface collab-dynamic-highlight
  '((t . (:background "gray90" :extend t)))
  "Face used for the dynamic highlight in the collab buffer.")

(defface collab-host '((t . (:inherit bold)))
  "Face used for the server line in the collab buffer.")

(defface collab-file '((t . (:inherit default)))
  "Face used for files in the collab buffer.")

(defvar collab-rpc-timeout 3
  "Timeout in seconds to wait for connection to collab process.")

(defvar collab-connection-timeout 30
  "Timeout in seconds to wait for connection to remote hosts.
Connecting to remote hosts might be slow since hosts might need
to perform NAT traversal.")

(defvar collab-command "~/p/collab/target/debug/collab"
  "Path to the collab process executable.")

(defvar collab-default-signaling-server "s.collab-mode.org"
  "Default signaling server’s address.

Signaling server is for NAT-traversal and host authentication.")

(defvar collab-connection-type '(socket 7701)
  "How to connect to the collab process.
The value can be ‘pipe’, meaning use stdio for connection,
or (socket PORT), meaning using a socket connection, and PORT
should be the port number.")

(defvar collab-known-hosts nil
  "Stores the hosts that you can connect to.

Used to provide a list of options when connecting to remotes.

Each key is HOST-ID in the form <name>::<cert-hash>. Each value is a
transport config. One of:

  (:SCTP (:signalingAddr ADDR))

  Meaning to connect via the signaling server at ADDR.

  (:SshStdio (:sshHost \"user@host\"
              :command [\"collab-mode\" \"envoy\"]
              :projects [(:name \"demo\" :path \"/abs/path\") ...]))

  Meaning to connect by ssh on user@host and run COMMAND on the remote
  in envoy mode. PROJECTS is the set of projects that the envoy should
  expose.")

(defvar collab--hosts-to-connect-to nil
  "Hosts to keep connection with for this session.

The value should have the same form as ‘collab-known-hosts’: an alist of
host id and host config.")

(defvar collab-hasty-p nil
  "If t, buffer changes are sent to collab process immediately.
For changes to this variable to take affect, you need to
reconnect to the doc.")

(defvar collab--verbose nil
  "If non-nil, print debugging information.")

(defvar collab--send-ops-threshold 20
  "Collab-mode send ops when pending ops is this long.
The measured length is the text length, not ops count.")

(defvar collab-backup-directory "/tmp/collab-mode"
  "Remote files are wrote to this directory as a local backup.
Under this directory, the first level sub-directories are each remote
host, then each project, then files.")

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

(defmacro collab--push-with-limit (elm seq max)
  "Push ELM to SEQ, if SEQ is longer than MAX, truncate it."
  `(progn
     (push ,elm ,seq)
     (when (> (length ,seq) ,max)
       (setcdr (nthcdr (max (1- ,max) 0) ,seq) nil))))

(defun collab--msg-event (face message &optional no-message)
  "Print MESSAGE in FACE and add it to ‘collab--events’.
If NO-MESSAGE is non-nil, don’t message, only and to events."
  (unless no-message
    (message (propertize message 'face face)))
  (collab--push-with-limit
   (format "%s %s"
           (propertize (format-time-string "%I:%M:%S") 'face 'collab-host)
           (propertize message 'face face))
   collab--events 30))

(defun collab--id-hash (host-id)
  "Return the cert-hash portion of HOST-ID.

HOST-ID is <name>::<cert hash>. If name doesn’t exist, we consider the whole
thing cert hash."
  (let ((idx (string-search "::" host-id)))
    (if idx (substring host-id (+ idx 2)) host-id)))

(defun collab--id-name (host-id)
  "Return the name portion of HOST-ID, or nil if there isn't one."
  (let ((idx (string-search "::" host-id)))
    (when (and idx (> idx 0))
      (substring host-id 0 idx))))

(defun collab--prettify-host-id (host-id)
  "Return HOST-ID with the cert-hash portion displayed as '@'.

HOST-ID should be <name>::<cert-hash> but name might not exist. The full
host-id is preserved in the string, only the display is affected. A
tooltip shows the full host-id on hover."
  (let ((name (collab--id-name host-id)))
    (cond
     ;; Name exists, show name.
     (name
      (let* ((full-str (copy-sequence host-id))
             (split (length name)))
        (put-text-property (+ split 4) (- (length full-str) 2)
                           'display ".." full-str)
        (put-text-property 0 (length full-str)
                           'help-echo host-id full-str)
        full-str))
     ;; No name, show the first and last byte.
     (t
      (let ((full-str (copy-sequence host-id)))
        (when (> (length full-str) 5)
          (put-text-property 4 (- (length full-str) 2)
                             display ".." full-str)
          (put-text-property 0 (length full-str)
                             'help-echo host-id full-str))
        full-str)))))

(defvar collab--host-idx-hosts nil
  "A list of hosts we encountered. Purely for ‘collab--host-idx’.")

(defun collab--host-idx (host-id)
  "Get HOST-ID’s index.

Don’t worry about how the index is calculated. Just know that you can
use the index as an unique numerical id for the host for the current
session."
  (or (seq-position collab--host-idx-hosts host-id)
      (setq collab--host-idx-hosts
            (append collab--host-idx-hosts (list host-id)))
      (1- (length collab--host-idx-hosts))))

(defmacro collab--save-excursion (&rest body)
  "Save position, execute BODY, and restore point.
Point is not restored in the face of non-local exit."
  `(let ((pos (point-marker)))
     ,@body
     (goto-char pos)))

;;; Host data

(defun collab--all-hosts (connection-state)
  "Return the host ids reported in CONNECTION-STATE plus ourself.

CONNECTION-STATE is the full ‘ConnectionState’ response plist; we pluck
its ‘:connections’ vector and map each entry to its host id."
  (cons collab--my-host-id
        (seq-map (lambda (entry) (plist-get entry :hostId))
                 (plist-get connection-state :connections))))

(defun collab--host-data (host-id connection-state)
  "Return host data (transport config) of HOST-ID from CONNECTION-STATE.

Return nil for local host and hosts not in CONNECTION-STATE."
  (if (equal host-id collab--my-host-id)
      nil
    ;; Use ‘seq-find’ which works with arrays.
    (let ((entry (seq-find (lambda (entry)
                             (equal (plist-get entry :hostId) host-id))
                           connection-state)))
      (if entry
          (plist-get entry :transport)
        nil))))

(defun collab--host-state (host-id connection-state)
  "Return connection state of HOST-ID in CONNECTION-STATE.

Our own HOST-ID is always “Connected”.
For all possible states, see ‘collab--connection-state-req’.

Return nil if no info for HOST-ID is found."
  (if (equal host-id collab--my-host-id)
      "Connected"
    ;; Use ‘seq-find’ which works with arrays.
    (let ((entry (seq-find (lambda (entry)
                             (equal (plist-get entry :hostId) host-id))
                           connection-state)))
      (if entry
          (plist-get entry :state)
        nil))))

(defun collab--host-retry-secs (host-id connection-state)
  "Return seconds until next reconnect attempt for HOST-ID, or nil.

Only set when the host is in the “Disconnected” state. See
‘collab--connection-state-req’."
  (unless (equal host-id collab--my-host-id)
    (let ((entry (seq-find (lambda (entry)
                             (equal (plist-get entry :hostId) host-id))
                           connection-state)))
      (plist-get entry :nextRetryInSecs))))

;;; Icons

(defvar collab--load-directory
  (file-name-directory (or load-file-name buffer-file-name))
  "Directory in which collab.el resides.")

(define-icon collab-status-on nil
  `((image ,(concat collab--load-directory "/dot_medium_16.svg")
           :face success
           :height (0.9 . em)
           :ascent 92)
    (symbol "•")
    (text "*"))
  "Icon for collab on status indicator."
  :version "30.1"
  :help-echo "ON")

(define-icon collab-status-off nil
  `((image ,(concat collab--load-directory "/dot_medium_16.svg")
           :face error
           :height (0.9 . em)
           :ascent 92)
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
    (106 . NetworkError)
    (113 . BadRequest)
    (114 . NotConnected))
  "An alist of JSONRPC error codes.")

(defun collab--error-code-jsonrpc-error-p (code)
  "Return non-nil if CODE represents a JSONRPC error."
  (<= -32768 code -32000))

(defmacro collab--catch-error (msg &rest body)
  "Execute BODY, catch jsonrpc errors.
If there’s an error, print “collab MSG: ERROR-DESCRIPTION”,
and append the error to collab error buffer (*collab errors*).
DocFatal errors disable ‘collab-monitored-mode’ and surface as a
warning; other errors are signaled with ‘user-error’ and leave the
mode intact.
MSG should be something like “can’t do xxx”."
  (declare (indent 1) (debug (sexp &rest form)))
  (let ((err (gensym))
        (code (gensym)))
    `(condition-case ,err
         (progn
           ,@body)
       ((debug error)
        (let ((,code (alist-get 'jsonrpc-error-code (cdr-safe ,err))))
          (if (eq (alist-get ,code collab--error-code-alist) 'DocFatal)
              (progn
                (when collab-monitored-mode
                  (collab--disable))
                (display-warning 'collab
                                 (format "collab %s: %s" ,msg ,err)))
            (user-error "collab %s: %s" ,msg ,err)))))))

(defmacro collab--catch-error-no-signal (msg &rest body)
  "Execute BODY, catch jsonrpc errors.
If there’s an error, print “collab MSG: ERROR-DESCRIPTION”,
and append the error to collab error buffer (*collab errors*).
DocFatal errors disable ‘collab-monitored-mode’ and surface as a
warning; other errors are signaled with ‘user-error’ and leave the
mode intact.
MSG should be something like “can’t do xxx”."
  (declare (indent 1) (debug (sexp &rest form)))
  (let ((err (gensym))
        (code (gensym)))
    `(condition-case ,err
         (progn
           ,@body)
       ((debug error)
        (let ((,code (alist-get 'jsonrpc-error-code (cdr-safe ,err))))
          (if (eq (alist-get ,code collab--error-code-alist) 'DocFatal)
              (progn
                (when collab-monitored-mode
                  (collab--disable))
                (display-warning 'collab
                                 (format "collab %s: %s" ,msg ,err)))
            (message "collab %s: %s" ,msg ,err)))))))

(defvar collab--jsonrpc-connection)
(defvar collab--file)
(defun collab--check-precondition ()
  "Check ‘collab--jsonrpc-connection’ and ‘collab--file’.

If they are invalid, turn off ‘collab-monitored-mode’ and raise
an error. This function should be called before any interactive
command that acts on a collab-monitored buffer."
  (unless collab--jsonrpc-connection
    (collab-monitored-mode -1)
    (display-warning 'collab "Connection to collab host broke"))
  (unless collab--file
    (collab-monitored-mode -1)
    (display-warning
     'collab "Current buffer doesn’t have a doc id or host id")))


;;; Cursor tracking

(defvar-local collab--cursor-ov-alist nil
  "An alist mapping user’s host id to cursor overlays.")

(defvar-local collab--my-site-id nil
  "My site-id.")

(defvar collab--sync-cursor-timer nil
  "Global idle timer for sending cursor position.")

(defun collab--move-cursor (host-id pos &optional mark)
  "Move user (HOST-ID)’s cursor overlay to POS.
If MARK non-nil, show active region."
  (when (and (not (eq host-id collab--my-host-id))
             (<= (point-min) pos (point-max)))
    (save-restriction
      (widen)
      (let* ((idx (mod (collab--host-idx host-id)
                       (length collab-cursor-colors)))
             (color (nth idx collab-cursor-colors))
             (foreground (if (> 0.5 (rainbow-x-color-luminance color))
                             "white" "black"))
             (region-color (collab--blend-color
                            (face-attribute 'default :background)
                            color 0.2))
             (face `(:background ,color :foreground ,foreground))
             (region-face `(:background ,region-color))
             (ov (or (alist-get host-id collab--cursor-ov-alist nil nil #'equal)
                     (let ((ov (make-overlay (min (1+ pos) (point-max))
                                             (min (+ pos 2) (point-max))
                                             nil t nil)))
                       (overlay-put ov 'face face)
                       (push (cons host-id ov) collab--cursor-ov-alist)
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
(defvar collab--prev-sent-cursor-pos)

(defun collab--send-cursor-pos ()
  "Send cursor position if current buffer is in collab mode."
  (when (and collab-monitored-mode
             collab--file
             (not (eq collab--prev-sent-cursor-pos (point))))
    (let ((file-key (collab--encode-filename collab--file)))
      (collab--catch-error "can’t send cursor position to remote"
        (collab--send-info-req
         collab--file
         (if (region-active-p)
             `( :type "common.pos" :point ,(point)
                :mark ,(mark))
           `(:type "common.pos" :point ,(point))))
        (setq collab--prev-sent-cursor-pos (point))))))

;;; Global state

(defvar collab--my-host-id nil
  "Host id of our own server.")

(defvar collab--jsonrpc-connection nil
  "The JSONRPC connection to the collab process.")

(defvar collab--doc-id-table (make-hash-table :test #'equal)
  "A hash table that maps /HOST/PROJECT/PATH to its file-desc.")

(defvar collab--buffer-table (make-hash-table :test #'equal)
  "A has table that maps FILE-DESC to its corresponding buffer.")

(defvar collab--stashed-state-plist nil
  "Used for stashing local state when major mode changes.")

(defvar collab--dispatcher-timer-table
  (make-hash-table :test #'equal)
  "Hash table mapping FILE-DESC to dispatcher timers.")

(defvar collab--latest-resp-id-map (make-hash-table :test #'eq)
  "Maps response type symbol to max resp id we’ve received for that type.
Used to discard stale async responses. The id is the ‘float-time’
captured when the request was sent.")

(defvar collab--events nil
  "A list of events for display.
Each event is a string.")

;; Used for debugging.
(defvar collab--pause-auto-update nil
  "If non-nil, Emacs don’t apply changes from remote automatically.")

(defvar collab-global-setup-hook nil
  "Hook run once per fresh collab connection.
Runs inside ‘collab--connect-process’ after ‘Initialize’ succeeds and
after ‘collab--my-host-id’, the global monitoring mode, and the Tramp
backend have been set up. Use this for one-shot per-connection setup;
for per-buffer setup use ‘collab-monitored-mode-hook’ instead.")

(defun collab--shutdown-cleanup (&optional _connection)
  "Cleanup function ran when collab is shutdown."
  ;; Disable all collab buffers.
  (maphash (lambda (key val)
             (when (buffer-live-p val)
               (with-current-buffer val
                 (when collab-monitored-mode
                   (collab--disable)))))
           collab--buffer-table)
  (collab--global-monitoring-mode -1)
  (setq collab--doc-id-table (make-hash-table :test #'equal))
  (setq collab--buffer-table (make-hash-table :test #'equal))
  (setq collab--stashed-state-plist nil)
  (setq collab--dispatcher-timer-table (make-hash-table :test #'equal))
  (setq collab--events nil))

;;; Local state

(defvar-local collab--file nil
  "FILE-DESC for the current buffer.")

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

(defvar-local collab--send-ops-timer nil
  "Idle timer for sending out ops.")

(defvar-local collab--open-this-doc nil
  "When non-nil, open this doc when we finish refresh.
It should be a list (HOST-ID FILE-DESC FILENAME DIRECTORY-P).
‘collab--list-files-callback’ will open this doc if it’s in the
listed files.")

(defvar-local collab--hl-ov-1 nil
  "Overlay used for 1st level highlight.")

(defvar-local collab--hl-ov-2 nil
  "Overlay used for 2nd level highlight.")

(defvar-local collab--default-directory nil
  "Default directory for ‘collab-find-file’.
Stored as HOST/TYPE/PATH where TYPE is ‘p’ for project or ‘f’ for file.")

(defvar-local collab--prev-sent-cursor-pos nil
  "The last position sent-out to server.

If current position differs from this one, send a cursor position
message.")

(defvar-local collab--auto-undo-count 0
  "Number of consecutive auto-undos in current sequence.
When an undo returns empty edits due to transformation, we automatically
try another undo. This tracks how many we've done.")

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

(defun collab--ops-text-len (ops)
  "Return the text length of OPS.
Insertsion and deletion all counts toward the length."
  (let ((len 0))
    (dolist (op ops)
      (pcase op
        (`(ins ,_ ,str ,_)
         (setq len (+ len (length str))))
        (`(del ,_ ,str ,_)
         (setq len (+ len (length str))))))
    len))

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
                         (push (list 'ins (+ beg (length old))
                                     (substring current (length old))
                                     group-seq)
                               ops)
                       ;; Actual replace.
                       (push (list 'del beg old group-seq) ops)
                       (push (list 'ins beg current group-seq) ops)))))

             (setq collab--pending-ops
                   `(tracking-error
                     ,(format "Assertion failed: (<= rbeg beg end rend): %s"
                              (list rbeg beg end rend))))))
          (_ (setq collab--pending-ops
                   `(tracking-error
                     ,(format "Unexpected ‘collab--changing-region’ shape: %s"
                              collab--changing-region)))))))

    ;; Update state.
    (setq collab--changing-region nil)
    (collab--push-to-pending-ops ops)

    (run-hooks 'collab--after-change-hook)))

(defun collab--after-change-default ()
  "Default send-ops behavior: setup an idle timer to send ops."
  (when collab--send-ops-timer
    (cancel-timer collab--send-ops-timer))
  (let ((buf (current-buffer)))
    ;; Immediately send out pending ops if it’s long enough. Otherwise
    ;; send out after the idle delay.
    (if (> (collab--ops-text-len collab--pending-ops)
           collab--send-ops-threshold)
        (collab--send-ops-now)
      (setq collab--send-ops-timer
            (run-with-timer
             collab-send-ops-delay nil
             (lambda ()
               (when (buffer-live-p buf)
                 (with-current-buffer buf
                   (unwind-protect
                       (collab--send-ops-now)
                     (setq collab--send-ops-timer nil))))))))))

(defun collab--after-change-hasty ()
  "Send ops in every after-change function."
  (collab--send-ops-now))

(defun collab--send-ops-now ()
  "Run in ‘collab--send-ops-hook’, send ‘collab--pending-ops’."
  (if (eq (car collab--pending-ops) 'tracking-error)
      ;; TODO, send full buffer.
      (progn
        (when collab--verbose
          (message "Hit a tracking error! Abort")
          ;; (message "Hit a tracking error! Sending full buffer")
          (message "Reason: %s" (cadr collab--pending-ops)))
        (collab--disable))
    (unwind-protect
        (let ((ops (nreverse collab--pending-ops)))
          (when collab--verbose
            (message "Sending ops: %s" ops))
          (setq collab--pending-ops nil)
          (funcall collab--send-ops-fn ops))
      (when (numberp collab--group-seq)
        (cl-incf collab--group-seq))))
  (setq collab--prev-sent-cursor-pos (point)))

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
     (if (and (< (string-width str1) (+ collab--send-ops-threshold 5))
              ;; Make this (25) is longer than in ‘collab--after-change-default’.
              (eq pos2 (+ pos1 (length str1))))
         (list 'ins pos1 (concat str1 str2) seq1)
       nil))

    (`((del ,pos1 ,str1 ,seq1) . (del ,pos2 ,str2 ,_))
     ;; Amalgamate if OP2 deletes right before OP1. Don’t amalgamate
     ;; if OP2 is long enough.
     (if (and (< (string-width str1) (+ collab--send-ops-threshold 5))
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
    (define-key map [remap save-buffer] #'collab-save-buffer)
    (define-key map [remap revert-buffer] #'collab-reset-from-disk)
    (define-key map [remap find-file] #'collab-find-file-bifurcated)
    map)
  "Keymap for ‘collab-monitored-mode’.")

(defvar collab--dispatcher-timer-table)
(define-minor-mode collab-monitored-mode
  "Collaborative monitor mode."
  :global nil
  :keymap collab-monitored-mode-map
  (if collab-monitored-mode
      (progn
        (collab--global-monitoring-mode)
        (add-hook 'change-major-mode-hook #'collab--rescue-state 0 t)
        (add-hook 'after-change-major-mode-hook
                  #'collab--maybe-recover 0)
        (add-hook 'before-change-functions
                  #'collab--before-change 0 t)
        (add-hook 'after-change-functions #'collab--after-change 0 t)
        ;; Disconnect when killing buffer to reset site seq
        ;; verification on the file host.
        (add-hook #'kill-buffer-hook #'collab-disconnect-buffer 0 t)
        (if collab-hasty-p
            (add-hook 'collab--after-change-hook #'collab--after-change-hasty 0 t)
          (add-hook 'collab--after-change-hook #'collab--after-change-default 0 t))

        (unless (member collab--mode-line mode-line-misc-info)
          (setq-local mode-line-misc-info
                      (cons collab--mode-line mode-line-misc-info)))

        ;; (setq-local buffer-undo-list t)

        (setq-local collab--pending-ops nil)
        (setq-local collab--ops-current-command nil)
        (setq-local collab--group-seq 1))

    ;; Disable.
    (remhash (collab--encode-filename collab--file) collab--dispatcher-timer-table)

    (remove-hook 'before-change-functions #'collab--before-change t)
    (remove-hook 'after-change-functions #'collab--after-change t)
    (remove-hook 'collab--after-change-hook #'collab--after-change-default)
    (remove-hook 'collab--after-change-hook #'collab--after-change-hasty)

    (remhash (collab--encode-filename collab--file) collab--buffer-table)

    (dolist (ov collab--cursor-ov-alist)
      (delete-overlay (cdr ov)))
    (setq-local collab--cursor-ov-alist nil)

    ;; Don’t set doc and host to nil, so that ‘collab-reconnect’ can
    ;; auto-resume.
    ;; (setq-local collab--file nil)

    (setq-local collab--my-site-id nil)))

(defun collab--tramp-filename (file-desc)
  "Return the /collab: Tramp filename for FILE-DESC.
The host id (HOST::CERT-HASH) contains colons, so we wrap it in
the IPv6-style `[...]` brackets that TRAMP recognises as a single
host token."
  (let ((host-id (plist-get file-desc :hostId))
        (project (plist-get file-desc :project))
        (file (plist-get file-desc :file)))
    (if (equal file "")
        (format "/collab:[%s]:/%s/" host-id project)
      (format "/collab:[%s]:/%s/%s" host-id project file))))

(defun collab--enable (file-desc my-site-id path)
  "Enable ‘collab-monitored-mode’ in the current buffer.
FILE-DESC is associated with the current buffer.
MY-SITE-ID is the site id of this editor.

PATH should be a string HOST/PROJECT/PATH returned by
‘collab--encode-filename’."
  (setq collab--file file-desc)
  (setq collab--my-site-id my-site-id)
  (setq collab--default-directory (collab--file-name-directory path))
  (let* ((tramp-name (collab--tramp-filename file-desc))
         (tramp-dir (or (file-name-directory tramp-name) tramp-name)))
    (setq buffer-file-name tramp-name)
    (setq default-directory tramp-dir))
  (puthash path collab--file collab--doc-id-table)
  (puthash (collab--encode-filename collab--file)
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
                   `( :file ,collab--file
                      :site-id ,collab--my-site-id))))

(defun collab--maybe-recover ()
  "Maybe reenable ‘collab’ after major mode change."
  (when-let* ((state (plist-get collab--stashed-state-plist
                                (current-buffer)))
              (file (plist-get state :file))
              (site-id (plist-get state :site-id)))
    (collab--enable file site-id (collab--encode-filename file))))

;;; Global monitoring

(defun collab--setup-file (file-desc)
  "Setup FILE-DESC for collab editing in the current buffer.

Get file content from server and enable monitoring mode. The current
buffer shouldn’t already be in monitored mode."
  (cl-assert (not collab-monitored-mode))
  (collab--catch-error (format "can't connect to %s" file-desc)
    (let* ((resp (collab--open-file-req file-desc "create"))
           (site-id (plist-get resp :siteId))
           (content (plist-get resp :content))
           (resp-file-desc (plist-get resp :file))
           (filename (plist-get resp :filename))
           (meta (plist-get resp :fileMeta))
           (suggested-major-mode (plist-get meta :emacs.majorMode))
           (path (collab--encode-filename resp-file-desc))
           (backup-path (collab--backup-filename file-desc))
           (inhibit-read-only t))

      (erase-buffer)
      (insert content)
      (goto-char (point-min))

      (if (and (stringp suggested-major-mode)
               (functionp (intern-soft suggested-major-mode)))
          (funcall (intern-soft suggested-major-mode))
        (let ((buffer-file-name filename))
          (set-auto-mode)))

      (collab--enable resp-file-desc site-id path))))

(defun collab--find-file-hook ()
  "Enable collab mode if this file is in a shared project."
  (let ((this-file-path (buffer-file-name))
        (conn-state
         (plist-get
          (collab--use-query 'ConnectionState
                             #'collab--connection-state-req
                             30)
          :data)))
    ;; THIS-FILE-PATH will be nil for directories.
    (when (and this-file-path
               (string-prefix-p "/" this-file-path)
               conn-state)
      (catch 'done
        (seq-doseq (proj (plist-get conn-state :projects))
          (let ((proj-path (plist-get proj :path))
                (proj-name (plist-get proj :name))
                (filename (file-name-nondirectory this-file-path)))
            (when (string-prefix-p proj-path this-file-path)
              ;; If it’s a newly created file, save it so the collab
              ;; server can find it.
              (let ((file-desc (collab--make-file-desc
                                collab--my-host-id
                                proj-name
                                (string-trim
                                 (substring this-file-path (length proj-path))
                                 "/"))))
                (collab--setup-file file-desc)
                (collab--msg-event
                 'info (format "Opening %s in shared project (%s), taking over"
                               filename proj-name))
                (throw 'done nil)))))))))

(define-minor-mode collab--global-monitoring-mode
  "Collaborative global monitoring mode.

DO NOT close this mode while collab-mode is enabled. This is for
automatically delegating to collab mode when you open a file that falls
under a shared project."
  :global t
  (if collab--global-monitoring-mode
      (progn
        ;; ‘find-file-hook’ doesn’t work, eglot uses
        ;; ‘after-change-major-mode-hook’ too.
        (add-hook 'after-change-major-mode-hook #'collab--find-file-hook 70)
        (setq collab--sync-cursor-timer
              (run-with-idle-timer 1 t #'collab--send-cursor-pos)))
    (remove-hook 'after-change-major-mode-hook #'collab--find-file-hook)
    (when collab--sync-cursor-timer
      (cancel-timer collab--sync-cursor-timer))
    (setq collab--sync-cursor-timer nil)))

;;; JSON-RPC

(defun collab--make-file-desc (host-id project file)
  "Create an FILE-DESC using HOST-ID PROJECT FILE.

FILE-DESC := (:hostId HOST-ID :project PROJECT-ID :file FILE)

FILE is the relative path within the project, or empty string for project root.
For standalone buffers, PROJECT should be \"_buffers\" and FILE is the buffer id."
  (cl-assert (stringp host-id))
  (cl-assert (stringp project))
  (cl-assert (stringp file))
  `(:hostId ,host-id :project ,project :file ,file))

(defun collab--file-desc-host-id (file-desc)
  "Return the host-id from FILE-DESC."
  (plist-get file-desc :hostId))

(defun collab--file-desc-parent (file-desc)
  "Return a file desc that's the parent directory of FILE-DESC.

Return nil if there’s no parent."
  (let ((host-id (plist-get file-desc :hostId))
        (project (plist-get file-desc :project))
        (file (plist-get file-desc :file)))
    (cond
     ;; Project root -> nil (no parent).
     ((equal file "") nil)
     ;; Standalone buffer -> _buffers project.
     ((equal project "_buffers")
      (collab--make-file-desc host-id "_buffers" ""))
     ;; File in project -> parent directory or project root.
     (t
      (let ((parent (file-name-directory file)))
        (if (or (equal parent "/") (null parent))
            (collab--make-file-desc host-id project "")
          (collab--make-file-desc host-id project
                                  (string-trim-right parent "/"))))))))

(defun collab--file-desc-name (file-desc)
  "Return the filename of FILE-DESC."
  (let ((project (plist-get file-desc :project))
        (file (plist-get file-desc :file)))
    (if (equal file "")
        project
      (file-name-nondirectory file))))

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
                                    #'collab--dispatch-notification
                                    :on-shutdown
                                    #'collab--shutdown-cleanup))
               (resp (jsonrpc-request conn 'Initialize nil)))

          (setq collab--my-host-id (plist-get resp :hostId))

          ;; Install the find-file hook so opening a file under a
          ;; declared project auto-enables ‘collab-monitored-mode’.
          ;; Mirrors the ‘(collab--global-monitoring-mode -1)’ in
          ;; ‘collab--shutdown-cleanup’.
          (collab--global-monitoring-mode)

          ;; Load the Tramp backend so the “collab” method gets
          ;; registered. Deferred to first connect so it isn’t paid
          ;; at file-load time.
          (require 'tramp-collab)

          (run-hooks 'collab-global-setup-hook)

          (when (and collab-accept-connection-on-startup
                     collab-default-signaling-server)
            (jsonrpc-notify conn 'AcceptConnection
                            `(:addr ,collab-default-signaling-server)))

          (when collab--jsonrpc-connection
            (jsonrpc-shutdown collab--jsonrpc-connection))
          (setq collab--jsonrpc-connection conn)))))

;;;; Dispatcher

(defun collab--cancel-get-ops-timer ()
  "If there’s a timer for getting remote ops, cancel it.
The purpose of this function is to cancel get ops timer when we
know that we’re gonna send ops (which gets remotes ops too) very
soon."
  (when-let* ((timer (gethash (collab--encode-filename collab--file)
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
    ('RemoteOpsArrived
     ;; TODO: Idle timer?
     (when (not collab--pause-auto-update)
       (let ((file-desc (plist-get params :file))
             (last-seq (plist-get params :lastSeq)))
         (let* ((file-key (collab--encode-filename file-desc))
                (buffer (gethash file-key collab--buffer-table))
                (timer (gethash file-key collab--dispatcher-timer-table)))
           (when timer (cancel-timer timer))
           (when (buffer-live-p buffer)
             (setq timer (run-with-timer
                          collab-receive-ops-delay nil
                          #'collab--request-remote-ops
                          buffer last-seq))
             (puthash file-key timer collab--dispatcher-timer-table))))))
    ('InfoReceived
     (let ((file (plist-get params :file))
           (host-id (plist-get params :sender))
           (value (plist-get params :info)))
       (pcase (plist-get value :type)
         ("common.pos"
          (let* ((pos (plist-get value :point))
                 (mark (plist-get value :mark))
                 (buf (gethash (collab--encode-filename file)
                               collab--buffer-table)))
            (when (buffer-live-p buf)
              (with-current-buffer buf
                (collab--move-cursor host-id pos mark))))))))
    ('AcceptStopped
     (with-current-buffer (collab--hub-buffer)
       (collab--msg-event
        'warning
        (format "Stopped accepting remote connections, because %s"
                (plist-get params :reason)))
       (collab--refetch 'ConnectionState
                        (lambda ()
                          (collab--connection-state-req)))
       (collab--hub-rerender)))
    ('AcceptingConnection
     (collab--msg-event 'success "Now accepting remote connections")
     ;; Refresh accept state.
     (collab--refetch 'ConnectionState
                      (lambda ()
                        (collab--connection-state-req)))
     (collab--hub-rerender))
    ('ConnectionProgress
     (collab--msg-event 'default
                        (format "Connection to %s: %s"
                                (collab--prettify-host-id
                                 (plist-get params :hostId))
                                (plist-get params :message)))
     (collab--refetch 'ConnectionState #'collab--connection-state-req)
     (collab--hub-rerender))
    ('Connected
     (collab--refetch 'ConnectionState #'collab--connection-state-req)
     (let ((host-id (plist-get params :hostId))
           (hub-buffer (collab--hub-buffer)))
       ;; Send an async list files request and update collab hub. This
       ;; might be unnecessary but it doesn’t hurt. But don’t send
       ;; files list request if it’s just from a remote that connected
       ;; to US — those won’t be in our address book.
       (when (and (buffer-live-p hub-buffer)
                  (assoc host-id collab-known-hosts))
         (collab--list-files-req
          nil host-id
          (lambda (status resp)
            ;; This calls ‘collab--hub-rerender’.
            (collab--list-files-callback host-id nil status resp))))
       ;; Open the doc we were trying to open before connecting.
       (when (and collab--open-this-doc
                  (equal host-id (car collab--open-this-doc)))
         (pcase-let ((`(,_ ,file-desc ,filename ,directory-p)
                      collab--open-this-doc))
           (setq collab--open-this-doc nil)
           (collab--open-thing file-desc filename directory-p)))))
    ('FailedToConnect
     ;; TODO
     )
    ('ConnectionBroke
     (let* ((host-id (plist-get params :hostId))
            (reason (plist-get params :reason))
            (message (plist-get params :message))
            (err (string-join
                  (list (format "Connection to %s broke"
                                (collab--prettify-host-id host-id))
                        (if (and reason (not (equal reason "")))
                            (concat ", " reason)
                          "")
                        (if (and message (not (equal message "")))
                            (concat ", " message)
                          "")))))
       (collab--msg-event 'error err)
       (when (buffer-live-p (collab--hub-buffer))
         (collab--hub-render))))
    ('Connecting
     ;; Don’t need to do anything, we’ll get a connection progress
     ;; very soon.
     )
    ('FileListUpdated
     ;; TODO
     )
    ('FileMoved
     ;; TODO
     )
    ('FileDeleted
     (collab--msg-event 'info (plist-get params :message) 'no-message)
     (let* ((file-desc (plist-get params :file))
            (file-key (when file-desc
                        (collab--encode-filename file-desc)))
            (buf (when file-key
                   (gethash file-key collab--buffer-table))))
       (collab--msg-event 'info (format "%s deleted" file-key))
       (when buf
         (with-current-buffer buf
           (collab--disable)))
       (puthash file-key nil collab--buffer-table)))
    ('FileClosed
     (let* ((file-desc (plist-get params :file))
            (file-key (when file-desc
                        (collab--encode-filename file-desc)))
            (buf (when file-key
                   (gethash file-key collab--buffer-table))))
       (collab--msg-event 'info (format "%s closed" file-key))
       (when buf
         (with-current-buffer buf
           (collab--disable)))
       (puthash file-key nil collab--buffer-table)))
    ('UnimportantError
     (collab--msg-event 'info (plist-get params :message) 'no-message))
    ('InternalError
     (collab--msg-event 'error (plist-get params :message)))
    ('ErrorResponse
     (let* ((file (plist-get params :file))
            (filename (and file (collab--encode-filename file)))
            (code (plist-get params :code))
            (code-desc (or (alist-get code collab--error-code-alist)
                           (format "(Unrecognized code %s)" code)))
            (additional-message ""))
       (if (and (eq code-desc 'DocFatal) filename)
           (let ((buf (gethash filename collab--buffer-table)))
             (if buf
                 (with-current-buffer buf
                   (collab--disable))
               (setq additional-message
                     "...and we don’t even have the file open"))))
       (collab--msg-event
        'error
        (format "%s %s%s%s"
                code-desc
                (if filename (format "(%s) " filename) "")
                (plist-get params :message)
                additional-message))))))

;;;; Requests

(defun collab--list-files-req (dir host &optional callback)
  "Request for files on HOST.

If DIR is non-nil, it should be the FILE-DESC of the directory we want
to list. ‘collab--make-file-desc’ can create it.

Return a plist (:files [(:docDesc DOC-DESC :fileName FILE-NAME) ...]).

If CALLBACK is non-nil, this request is async and returns nil
immediately, when the response returns, CALLBACK is called with two
arguments, the status and the response object (not a list of files, but
the full plist response). STATUS can be ‘:success’, ‘:error’, and
‘:timeout’. For ‘:timeout’, response object is nil."
  (let ((conn (collab--connect-process))
        (request-object
         (if dir
             `(:dir ,dir)
           `(:hostId ,host))))
    (if callback
        ;; Async request. Stamp the request with ‘float-time’ and
        ;; drop the response if a newer one for the same type has
        ;; already been processed.
        (let* ((type (if dir 'ListFiles 'ListProjects))
               (req-id (float-time))
               (fresh-p (lambda ()
                          (let ((latest (gethash type collab--latest-resp-id-map 0)))
                            (when (> req-id latest)
                              (puthash type req-id collab--latest-resp-id-map)
                              t)))))
          (jsonrpc-async-request
           conn type
           request-object
           :success-fn (lambda (resp)
                         (when (funcall fresh-p)
                           (funcall callback :success resp)))
           :error-fn (lambda (resp)
                       (when (funcall fresh-p)
                         (funcall callback :error resp)))
           :timeout-fn (lambda ()
                         (when (funcall fresh-p)
                           (funcall callback :timeout nil)))
           :timeout collab-connection-timeout))
      ;; Sync request.
      (jsonrpc-request
       conn (if dir 'ListFiles 'ListProjects)
       request-object
       :timeout collab-rpc-timeout))))

(defun collab--connection-state-req ()
  "Get the connection state of each host.

Returns a plist

    (:connections CONNECTIONS
     :accepting ACCEPTING
     :live LIVE-FILES
     :connected CONNECTED-FILES)

CONNECTIONS is a vector of (:hostId HOST-ID :state CONNECTION-STATE
:nextRetryInSecs SECS), CONNECTION-STATE can be:

  - “Connected”: Connected and well.
  - “Connecting”: Trying to (re)connect.
  - “Disconnected”: Connection was established but then broke.
                    Server will try to reconnect.
  - “FailedToConnect”: Connection was never established.
                       Server doesn’t try to reconnect.

:nextRetryInSecs is only present when CONNECTION-STATE is
“Disconnected” and gives the seconds until the next reconnect attempt.

ACCEPTING is a vector of entries, each (:addr ADDR), one per
signaling server we have successfully bound to.

LIVE-FILES is a vector of (:file :subscribers filename :meta :seq),
CONNECTED-FILES is a vector of (:file :filename :meta)."
  (let* ((conn (collab--connect-process)))
    (jsonrpc-request
     conn 'ConnectionState nil
     :timeout collab-rpc-timeout)))

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

(defun collab--open-file-req (file-desc mode)
  "Connect to FILE-DESC on HOST.
MODE can be \"Create\" or \"Open\".
Return (:siteId SITE-ID :content CONTENT)."
  (let* ((conn (collab--connect-process)))
    (message (collab--fairy "Getting the file for ya! Timeout is (checking notes) %s seconds!"
                            (* 3 collab-rpc-timeout)))
    (let ((resp (jsonrpc-request
                 conn 'OpenFile
                 `( :file ,file-desc
                    :mode ,mode)
                 :timeout (* 3 collab-rpc-timeout))))
      (message (collab--fairy "Here it is!"))
      resp)))

(defun collab--disconnect-from-file-req (file-desc)
  "Disconnect from FILE-DESC."
  (let ((conn (collab--connect-process)))
    (jsonrpc-request conn 'DisconnectFromFile
                     `( :file ,file-desc)
                     :timeout collab-rpc-timeout)))

(defun collab--close-file-req (file-desc)
  "Close FILE-DESC."
  (let ((conn (collab--connect-process)))
    (jsonrpc-request conn 'CloseFile
                     `( :file ,file-desc)
                     :timeout collab-rpc-timeout)))

(defun collab--save-file-req (file-desc)
  "Save FILE-DESC to disk."
  (let ((conn (collab--connect-process)))
    (jsonrpc-request conn 'SaveFile
                     `( :file ,file-desc)
                     :timeout collab-rpc-timeout)))

(defun collab--reset-from-disk-req (file-desc)
  "Reload FILE-DESC from the owner’s on-disk content."
  (let ((conn (collab--connect-process)))
    (jsonrpc-request conn 'ResetFromDisk
                     `( :file ,file-desc)
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

(defun collab--delete-file-req (file-desc)
  "Delete FILE-DESC."
  (let ((conn (collab--connect-process)))
    (jsonrpc-request conn 'DeleteFile
                     `( :file ,file-desc)
                     :timeout collab-rpc-timeout)))

(defun collab--accept-connection-notif (signaling-addr)
  "Accept connections on SIGNALING-ADDR.
The trust list comes from the collab process's current
‘trusted_hosts’; manage it via ‘collab-config-edit-trusted-hosts’."
  (let ((conn (collab--connect-process)))
    (jsonrpc-notify conn 'AcceptConnection
                    `(:addr ,signaling-addr))))

(defun collab--stop-accepting-notif (signaling-addr)
  "Tell the collab process to stop accepting on SIGNALING-ADDR."
  (let ((conn (collab--connect-process)))
    (jsonrpc-notify conn 'StopAccepting
                    `(:addr ,signaling-addr))))

(defun collab--connect-notif (host-id host-data)
  "Connect to HOST-ID using transport info from HOST-DATA.

HOST-DATA is the value of an entry in `collab-known-hosts', matching the
wire shape of a `transportConfig'."
  (let ((conn (collab--connect-process)))
    (jsonrpc-notify conn 'Connect
                    `(:hostId ,host-id :transportConfig ,host-data))))

(defun collab--update-config-req (trusted-hosts permissions)
  "Send UpdateConfig request.
TRUSTED-HOSTS is a list of trusted host-ids.
PERMISSIONS is a hash table mapping host-id to permission hash tables."
  (let ((conn (collab--connect-process))
        (config (make-hash-table :test #'equal)))
    ;; Convert to a vector.
    (puthash "trustedHosts" (vconcat trusted-hosts) config)
    (puthash "permission" permissions config)
    (jsonrpc-request conn 'UpdateConfig
                     `(:config ,config)
                     :timeout collab-rpc-timeout)))

;; FIXME
(defun collab--print-history-req (file-desc debug)
  "Print debugging history for FILE-DESC.
If DEBUG non-nil, print debug version."
  (let ((conn (collab--connect-process)))
    (jsonrpc-request conn 'PrintHistory
                     `( :file ,file-desc
                        :debug ,(if debug t :json-false))
                     :timeout collab-rpc-timeout)))

;; FIXME
(defun collab--send-info-req (file-desc info)
  "Send INFO to FILE-DESC.
INFO should be a plist JSON object. This request is async."
  (let ((conn (collab--connect-process)))
    (jsonrpc-notify conn 'SendInfo
                    `( :file ,file-desc
                       :info ,info))))

(defsubst collab--encode-op (op)
  "Encode Elisp OP into JSONRPC format."
  (pcase op
    (`(ins ,pos ,str ,group-seq)
     `( :op (:kind "Ins" :pos ,(1- pos) :content ,str)
        :groupSeq ,group-seq))

    (`(del ,pos ,str ,group-seq)
     `( :op (:kind "Del" :pos ,(1- pos) :content ,str)
        :groupSeq ,group-seq))))

(defsubst collab--apply-edit-instruction (instr &optional move-point)
  "Decode EditInstruction INSTR and apply it.
If MOVE-POINT non-nil, move point as the edit would."
  (let ((inhibit-modification-hooks t)
        (collab--inhibit-hooks t)
        (start (point-marker))
        (kind (plist-get instr :kind))
        (edits (plist-get instr :edits)))
    (save-restriction
      (widen)
      (pcase kind
        ("Ins"
         (mapc (lambda (edit)
                 (let ((pos (plist-get edit :pos))
                       (text (plist-get edit :content)))
                   (goto-char (1+ pos))
                   (insert text)))
               (reverse edits)))
        ("Del"
         (mapc (lambda (edit)
                 (let ((pos (plist-get edit :pos))
                       (text (plist-get edit :content)))
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
             `( :file ,collab--file
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

(defun collab--undo-instructions-empty-p (instructions)
  "Return t if all edits in INSTRUCTIONS have empty content.
INSTRUCTIONS is a vector of EditInstruction plists."
  (seq-every-p
   (lambda (instr)
     (let ((edits (plist-get instr :edits)))
       (seq-every-p
        (lambda (edit)
          (equal (plist-get edit :content) ""))
        edits)))
   instructions))

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
                             `( :file ,collab--file
                                :kind ,(if redo "Redo" "Undo"))
                             :timeout collab-rpc-timeout))
      (if-let* ((instructions (plist-get resp :ops))
                (context (plist-get resp :context)))
          (if (eq (length instructions) 0)
              (message "No more operations to %s" (if redo "redo" "undo"))
            ;; Only 'seq-map' can map over vector.
            (seq-map (lambda (instr)
                       (collab--apply-edit-instruction instr t))
                     instructions)
            (collab--send-ops
             `[( :op (:kind ,(if redo "Redo" "Undo") :context ,context)
                 :groupSeq ,collab--group-seq)]
             t)
            ;; Check if the edits are empty (due to transformation).
            (if (collab--undo-instructions-empty-p instructions)
                (if (< collab--auto-undo-count 5)
                    (progn
                      (message (format "The %s op is empty due to transformation, going one step further"
                                       (if redo "redo" "undo")))
                      (setq collab--auto-undo-count (1+ collab--auto-undo-count))
                      (run-with-timer 0 nil #'collab--undo redo))
                  ;; Hit the limit, show message and reset.
                  (message "The last %d %s were all empty due to transformation, stopping auto-retry here, but feel free to %s again"
                           collab--auto-undo-count
                           (if redo "redo" "undo")
                           (if redo "redo" "undo"))
                  (setq collab--auto-undo-count 0))
              ;; Edits are not empty, reset counter.
              (setq collab--auto-undo-count 0)))
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
    (collab--save-file-req collab--file))
  (message "Buffer saved"))

(defun collab-reset-from-disk ()
  "Discard local edits and reload the buffer from the owner’s disk.
The owner reads the file fresh and broadcasts the diff as ops, so the
buffer here updates through the normal op delivery path."
  (interactive)
  (collab--check-precondition)
  (collab--catch-error "Can’t reset buffer from disk"
    (collab--reset-from-disk-req collab--file))
  (message "Buffer reset from disk"))

;;; UI

;;;; Data

(defvar collab--cached-responses (make-hash-table :test #'equal)
  "A hash table of QUERY-KEY to QUERY-DATA.
QUERY-KEY is an arbitrary list that contains, eg, type of query, query
params. QUERY-DATA is the data returned by collab server or error, so
it’s a plist (:data DATA :error ERROR :fetch-time TIMESTAMP).

Currently used keys:

- CurrentMessage
- ConnectionState
- Connected
- (ListFiles host-id dir)")

(defun collab--invalidate-query (key)
  "Remove cached data for KEY."
  (puthash key nil collab--cached-responses))

(defun collab--use-query (key query-fn stale-time)
  "Return the cached data tagged under KEY.

Call QUERY-FN to fetch data if:
- The cached data is an error
- STALE-TIME seconds has passed since the cached data is retrieved
- There’s no cached data

Returns a plist of (:data DATA :error ERROR :fetch-time TIMESTAMP)."
  (let* ((cached-data (gethash key collab--cached-responses))
         (fetch-time (plist-get cached-data :fetch-time)))
    (when (or (memq :error cached-data)
              (and fetch-time
                   (> (time-to-seconds (time-since fetch-time)) stale-time))
              (null cached-data))
      (collab--refetch key query-fn))
    (gethash key collab--cached-responses)))

(defun collab--refetch (key query-fn)
  "Run QUERY-FN and save the result under KEY in ‘collab--cached-responses’.

Key can be any object. Return the result. The returned value is
either (:data RESPONSE) or (:error ERROR)."
  (condition-case err
      (progn
        (let ((resp (funcall query-fn)))
          (puthash key (list :data resp) collab--cached-responses)))
    (error (puthash key
                    (list :error (format "%s" err))
                    collab--cached-responses))))

;;;; Drawing the hub UI

(defun collab--hub-buffer ()
  "Return the hub buffer."
  (get-buffer-create "*collab hub*"))

;; This function becomes unused after we changed to use
;; ‘collab--cached-responses’ + ‘collab--hub-render’. But we might use
;; it again in the future.
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
    (file-desc filename directoryp &optional display-name face prefix)
  "Insert FILE-DESC with FILENAME.

Insert as a directory if DIRECTORYP non-nil. If DISPLAY-NAME non-nil,
use that as the display name.

If FACE given, use it for the inserted file. If PREFIX given, prefix
each file with it."
  (let ((beg (point)))
    (insert (or prefix "")
            (propertize (or display-name filename)
                        'face (or face (if directoryp 'dired-directory nil))))
    (add-text-properties
     beg (point) `( collab-file-desc ,file-desc
                    collab-filename ,filename
                    collab-directory-p ,directoryp))))

(defun collab--insert-host-1 (host resp status &optional error retry-secs)
  "Insert HOST, its STATUS, and its files in RESP.

STATUS can be “Connected”, “Disconnected”, “Connecting”,
“FailedToConnect”, or nil (meaning don’t show status).

RESP can be the response object of ListFiles request or nil.
Insert (empty) if RESP is nil; insert ... if STATUS is not ‘up’.

If ERROR (string) is non-nil, also insert the error. ERROR
shouldn’t end with a newline.

If RETRY-SECS is non-nil and STATUS is “Disconnected”, also show
the seconds until the next reconnect attempt."
  (let* ((beg (point))
         (files (seq-map #'identity (plist-get resp :files))))
    ;; 1. Insert host line.
    (insert (propertize (if (equal host collab--my-host-id)
                            "My files"
                          (collab--prettify-host-id host))
                        'face 'collab-host
                        'collab-host-line t))
    ;; 1.1 Insert status.
    (unless (equal host collab--my-host-id)
      (insert (pcase status
                ("Connecting" (propertize " CONNECTING" 'face 'shadow))
                ("Connected" (propertize " UP" 'face 'success))
                ("Disconnected"
                 (propertize
                  (if retry-secs
                      (format " DOWN (retry in %ds)" retry-secs)
                    " DOWN")
                  'face 'error))
                ("FailedToConnect" (propertize " DOWN" 'face 'error))
                (_ ""))))
    (insert (propertize "\n" 'line-spacing 0.4))
    ;; 2. Insert files.
    (pcase status
      ("Connected" (if (null files)
                       (insert (propertize "(empty)\n" 'face 'shadow))
                     (dolist (entry files)
                       (let* ((file-desc (plist-get entry :file))
                              (name (plist-get entry :filename))
                              (directoryp (not (eq (plist-get entry :isDirectory)
                                                   :json-false))))
                         (collab--insert-file
                          file-desc name directoryp nil 'default " ")
                         (insert "\n")))))
      ;; Not connected. Getting files async or can’t get files.
      (_ (insert (propertize "...\n" 'face 'shadow))))
    ;; 3. Insert error.
    (when error
      (insert (propertize (string-trim error) 'face 'shadow) "\n"))
    ;; 4. Mark host section.
    (add-text-properties beg (point)
                         `(collab-host-id ,host))))

;; (defun collab--override-host-data (host-id data)
;;   "Override host data for HOST-ID in ‘collab--hub-data’ with DATA."
;;   (let* ((hosts (plist-get collab--hub-data :hosts))
;;          (new-hosts
;;           (mapcar (lambda (old-host)
;;                     (if (equal (plist-get old-host :host) host-id)
;;                         data
;;                       old-host))
;;                   hosts)))
;;     (plist-put collab--hub-data :hosts new-hosts)))

(defun collab--list-files-callback (host dir status resp)
  "The callback function for inserting HOST and its files.

DIR is the directory FILE-DESC in which we’re listing files, or nil for
top-level projects.

STATUS should be one of ‘:success’, ‘:error’, or ‘:timeout’. For
‘:success’, RESP is the plist response object of ListFiles. For
‘:error’, RESP is an error object that contains ‘code’,
‘message’, ‘data’. For ‘:timeout’, resp is nil.

This function finds the section for the host in the CURRENT
BUFFER (make sure it’s the hub buffer), and populates the file
list."
  (pcase status
    (:success
     (puthash (list 'ListFiles host dir) (list :data resp) collab--cached-responses))
    (:error
     (let ((err (format "Failed to list files of %s. %s"
                        (collab--prettify-host-id host)
                        (or (plist-get resp :message) ""))))
       (puthash (list 'ListFiles host dir) (list :error err) collab--cached-responses)
       (collab--msg-event 'error err)))
    (:timeout
     (let ((err (format "Timed out listing files of %s"
                        (collab--prettify-host-id host))))
       (puthash (list 'ListFiles host dir) (list :error err) collab--cached-responses)
       (collab--msg-event 'error err))))
  (collab--hub-rerender))

(defun collab--prop-section (prop)
  "Return the (BEG . END) of the range that has PROP."
  (save-excursion
    (let ((forward-prop (text-property-search-forward prop))
          (backward-prop (text-property-search-backward prop)))
      (cons (prop-match-beginning backward-prop)
            (prop-match-end forward-prop)))))

(defvar collab-hub-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "RET") #'collab--open-thing-at-point)
    (define-key map (kbd "x") #'collab--delete-doc)
    (define-key map (kbd "k") #'collab--disconnect-or-close)
    (define-key map (kbd "l") #'collab-share-link)
    (define-key map (kbd "g") #'collab--refresh)
    (define-key map (kbd "A") #'collab--toggle-accept-connection)
    (define-key map (kbd "C") #'collab-connect)
    (define-key map (kbd "+") #'collab-share)
    (define-key map (kbd "H") #'collab-list-docs)
    (define-key map (kbd ",") #'collab-config)

    (define-key map (kbd "n") #'next-line)
    (define-key map (kbd "p") #'previous-line)

    (define-key map [remap find-file] #'collab-find-file)
    map)
  "Keymap for ‘collab-hub-mode’.")

(define-derived-mode collab-hub-mode special-mode "Collab"
  "Collaboration mode."
  (collab--global-monitoring-mode)
  (setq-local line-spacing 0.2
              eldoc-idle-delay 0.8
              collab--default-directory "/")
  (add-hook 'eldoc-documentation-functions #'collab--eldoc 0 t)
  (add-hook 'post-command-hook #'collab--dynamic-highlight)
  (eldoc-mode))

(defun collab--hub-render ()
  "Render collab mode buffer.
Also insert ‘collab--current-message’ if it’s non-nil."
  ;; If ConnectionState can fail, we’re probably not connected at all,
  ;; so just get the data. Assume someone has refetched
  ;; ConnectionState data before calling this function.
  (let* ((conn-state-data (plist-get
                           (gethash 'ConnectionState collab--cached-responses)
                           :data))
         (connected (plist-get
                     (gethash 'Connected collab--cached-responses)
                     :data))
         (accepting (plist-get conn-state-data :accepting))
         (hosts (collab--all-hosts conn-state-data))
         (current-message (gethash 'CurrentMessage collab--cached-responses)))
    ;; 1. Insert headers.
    (insert "Connection: ")
    (insert (if connected
                (propertize "UP" 'face 'success)
              (propertize "DOWN" 'face 'error))
            "\n")
    (insert (format "Connection type: %s\n" collab-connection-type))

    ;; 1.5
    (insert (if (> (seq-length accepting) 0)
                (propertize "\nAccepting remote connections" 'face 'bold)
              (substitute-command-keys
               "Not accepting remote connections (press \\[collab--toggle-accept-connection] to accept)"))
            "\n")
    (when (> (seq-length accepting) 0)
      (seq-doseq (entry accepting)
        (insert (format "AT %s/%s\t"
                        (plist-get entry :addr)
                        (propertize
                         (collab--prettify-host-id
                          (or collab--my-host-id "???"))
                         'face 'italic))
                (propertize "YES" 'face 'success))))
    (insert "\n\n")

    ;; 2. Insert notice message, if any.
    (when current-message
      (delete-char -1)
      (let ((beg (point)))
        (insert "\n" (string-trim current-message) "\n\n")
        (font-lock-append-text-property beg (point) 'face 'diff-added))
      (insert "\n"))

    ;; 3. Insert each host and files.
    (dolist (host hosts)
      (if (not connected)
          (collab--insert-host-1 host nil nil)
        (let* ((entries (plist-get conn-state-data :connections))
               (list-files-resp (gethash (list 'ListFiles host nil)
                                         collab--cached-responses))
               (state (collab--host-state host entries))
               (retry-secs (collab--host-retry-secs host entries)))
          (collab--insert-host-1
           host (plist-get list-files-resp :data) state nil retry-secs)))
      (insert "\n"))
    (insert "\n")

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
PRESS \\[collab-list-docs] TO SHOW ALL DOCS
PRESS \\[collab-share] TO SHARE A FILE/PROJECT
PRESS \\[collab-connect] TO CONNECT TO A REMOTE DOC
PRESS \\[collab-config] TO SET TRUSTED HOSTS AND PERMISSION
PRESS \\[collab--toggle-accept-connection] TO TOGGLE ACCEPTING REMOTE CONNECTIONS\n"))
    (insert "\n\n")

    ;; 6. Events.
    (when collab--events
      (insert "Recent events:\n\n")
      (dolist (event collab--events)
        (insert event "\n"))
      (insert "\n"))))

(defun collab--hub-refetch ()
  "Recompute data for collab hub."
  (let (connected connection-state fetched)
    (puthash 'CurrentMessage nil collab--cached-responses)
    (setq connected (condition-case nil
                        (progn
                          (collab--connect-process)
                          t)
                      (error nil)))
    (puthash 'Connected `(:data ,connected) collab--cached-responses)

    (setq connection-state
          (plist-get (collab--refetch
                      'ConnectionState #'collab--connection-state-req)
                     :data))
    (when (and connected connection-state)
      ;; Fetch the file list for ourselves.
      (when collab--my-host-id
        (collab--list-files-req
         nil collab--my-host-id
         (lambda (status resp)
           (collab--list-files-callback
            collab--my-host-id nil status resp))))
      ;; Fetch file list for connected remote hosts.
      (seq-doseq (entry (plist-get connection-state :connections))
        (let ((host-id (plist-get entry :hostId))
              (state (plist-get entry :state)))
          (push host-id fetched)
          (when (equal state "Connected")
            ;; Connected, get files async.
            (collab--list-files-req
             nil host-id
             (lambda (status resp)
               (collab--list-files-callback host-id nil status resp))))))
      ;; If any host in ‘collab--hosts-to-connect-to’ isn’t connected,
      ;; try to connect to it.
      (dolist (host collab--hosts-to-connect-to)
        (let ((host-id (car host))
              (host-data (cdr host)))
          (when (not (member host-id fetched))
            (collab--connect-notif host-id host-data)))))))

;;;; Docs listing

(defun collab--docs-buffer ()
  "Return the docs buffer."
  (get-buffer-create "*collab docs*"))

(defvar collab-docs-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "RET") #'collab--open-thing-at-point)
    (define-key map (kbd "x") #'collab--delete-doc)
    (define-key map (kbd "k") #'collab--disconnect-or-close)
    (define-key map (kbd "g") #'collab--refresh)
    (define-key map (kbd "C") #'collab-connect)
    (define-key map (kbd "+") #'collab-share)
    (define-key map (kbd "H") #'collab-hub)

    (define-key map (kbd "n") #'next-line)
    (define-key map (kbd "p") #'previous-line)

    (define-key map [remap find-file] #'collab-find-file)
    map)
  "Keymap for ‘collab-hub-mode’.")

(define-derived-mode collab-docs-mode special-mode "Collab docs"
  "Collaboration doc listing mode."
  (collab--global-monitoring-mode)
  (setq-local line-spacing 0.2
              eldoc-idle-delay 0.8)
  (add-hook 'eldoc-documentation-functions #'collab--eldoc 0 t)
  (add-hook 'post-command-hook #'collab--dynamic-highlight)
  (eldoc-mode))

(defun collab--docs-render ()
  "Render collab docs buffer."
  ;; If ConnectionState can fail, we’re probably not connected at all,
  ;; so just get the data.
  (let* ((conn-state-data (plist-get
                           (gethash 'ConnectionState collab--cached-responses)
                           :data))
         (connected (plist-get
                     (gethash 'Connected collab--cached-responses)
                     :data)))
    (if (not connected)
        (insert (collab--fairy "Alas, no connection to the server,\nto found out what happend, let’s go to the hub buffer!\n\n")
                (substitute-command-keys
                 "PRESS \\[collab-hub] TO GO TO HUB BUFFER\n"))
      (insert (propertize "Files hosted by our server:\n"
                          'face 'dired-header))
      (if (eq 0 (seq-length (plist-get conn-state-data :live)))
          (insert (propertize "(no files yet)\n" 'face 'shadow))
        (seq-doseq (entry (plist-get conn-state-data :live))
          (let ((file (plist-get entry :file))
                (subscribers (plist-get entry :subscribers))
                (filename (plist-get entry :filename))
                (seq (plist-get entry :seq)))
            (collab--insert-file file filename nil)
            (put-text-property (pos-bol) (pos-eol) 'collab-hosted-file-p t)
            (insert (propertize (format " (%s connection%s)"
                                        (seq-length subscribers)
                                        (if (eq 1 (seq-length subscribers))
                                            "" "s"))
                                'face 'shadow))
            (insert "\n"))))
      (insert "\n")
      (insert (propertize "Files opened:\n" 'face 'dired-header))
      (if (eq 0 (seq-length (plist-get conn-state-data :connected)))
          (insert (propertize "(no files yet)\n" 'face 'shadow))
        (seq-doseq (entry (plist-get conn-state-data :connected))
          (let ((file (plist-get entry :file))
                (filename (plist-get entry :filename)))
            (collab--insert-file file filename nil)
            (insert "\n")))))))

;;;; Dynamic highlight

(defun collab--dynamic-highlight ()
  "Highlight host and file at point."
  (let ((ov (or collab--hl-ov-1
                (let ((ov (make-overlay 1 1)))
                  (overlay-put ov 'face 'collab-dynamic-highlight)
                  (setq collab--hl-ov-1 ov)))))
    (cond
     ((get-text-property (point) 'collab-file-desc)
      (let ((range (collab--prop-section 'collab-file-desc)))
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

;;; Eldoc

(defun collab--eldoc (callback)
  "Displays help information at point.
Used for ‘eldoc-documentation-functions’, calls CALLBACK
immediately."
  (let ((file-desc (get-text-property (point) 'collab-file-desc))
        (host-id (get-text-property (point) 'collab-host-id)))
    (cond
     (file-desc
      (funcall
       callback
       (substitute-command-keys
        (concat
         "\\[collab--open-thing-at-point] → Open  "
         "\\[collab--delete-doc] → Delete  "
         "\\[collab--disconnect-or-close] → Disconnect  "
         (if (derived-mode-p 'collab-hub-mode)
             "\\[collab-share-link] → Copy share link"
           "")))))
     (host-id
      (funcall callback (substitute-command-keys
                         "\\[collab-share] → Share File/directory")))
     (t
      (funcall callback (substitute-command-keys
                         "\\[collab--refresh] → Refresh"))))))

;;; Dired

(defvar collab-dired-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "RET") #'collab--open-thing-at-point)
    (define-key map (kbd "x") #'collab--delete-doc)
    (define-key map (kbd "k") #'collab--disconnect-from-doc)
    (define-key map (kbd "g") #'collab--dired-refresh)

    (define-key map (kbd "n") #'next-line)
    (define-key map (kbd "p") #'previous-line)

    (define-key map [remap find-file] #'collab-find-file-bifurcated)
    map)
  "Keymap for ‘collab-dired-mode’.")

(define-derived-mode collab-dired-mode special-mode "Collab Dired"
  "Mode showing a shared directory."
  (font-lock-mode -1))

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
  "Draw the current 'collab-dired-mode' buffer from scratch."
  (when (and (derived-mode-p 'collab-dired-mode) collab--file)
    (collab--catch-error (format "can't connect to %s"
                                 (collab--prettify-host-id
                                  (collab--file-desc-host-id collab--file)))
      (let* ((file-desc collab--file)
             (host-id (collab--file-desc-host-id collab--file))
             (projectp (equal (plist-get file-desc :file) ""))
             (file-list-resp (collab--list-files-req file-desc host-id))
             (file-list (seq-map #'identity
                                 (plist-get file-list-resp :files)))
             (parent-file-desc (collab--file-desc-parent file-desc)))
        (insert (propertize (or (collab--encode-parent file-desc)
                                (collab--prettify-host-id host-id))
                            'face 'dired-header)
                ":\n")
        (insert ".\n")
        (if projectp
            (insert "..")
          (collab--insert-file
           parent-file-desc (collab--file-desc-name parent-file-desc) t ".."))
        (insert "\n")
        (if (null file-list)
            (insert (collab--fairy "There’s no files here!\n"))
          (dolist (entry file-list)
            (let* ((file-desc (plist-get entry :file))
                   (filename (plist-get entry :filename))
                   (directoryp (not (eq (plist-get entry :isDirectory) :json-false))))
              (collab--insert-file file-desc filename directoryp)
              (insert "\n"))))
        (put-text-property (point-min) (point-max)
                           'collab-host-id host-id)))))

;;; Interactive commands

(defun collab-shutdown ()
  "Shutdown the connection to the collab process."
  (interactive)
  (when collab--jsonrpc-connection
    ;; Shutdown function is called in the on-shutdown callback.
    (jsonrpc-shutdown collab--jsonrpc-connection)
    (setq collab--jsonrpc-connection nil)))

(defun collab--hub-refresh ()
  "Refresh collab hub buffer."
  (interactive)
  (collab--hub-refetch)
  (collab--hub-rerender))

(defun collab--docs-refresh ()
  "Refresh collab docs buffer."
  (interactive)
  (collab--refetch 'ConnectionState
                   (lambda ()
                     (collab--connection-state-req)))
  (collab--docs-rerender))

(defun collab--docs-rerender ()
  "Refresh collab docs buffer."
  (interactive)
  (with-current-buffer (collab--docs-buffer)
    (let ((inhibit-read-only t)
          (line (line-number-at-pos)))
      (erase-buffer)
      (collab--docs-render)
      (goto-char (point-min))
      (forward-line (1- line)))))

(defun collab--hub-rerender ()
  "Re-render hub buffer but don’t refetch files list."
  (with-current-buffer (collab--hub-buffer)
    (let ((inhibit-read-only t)
          (line (line-number-at-pos)))
      (erase-buffer)
      (collab--hub-render)
      (goto-char (point-min))
      (forward-line (1- line)))))

(defun collab--refresh ()
  "Refresh current buffer, works in both hub and dired mode."
  (interactive)
  (cond ((derived-mode-p 'collab-hub-mode)
         (collab--hub-refresh))
        ((derived-mode-p 'collab-dired-mode)
         (collab--dired-refresh))
        ((derived-mode-p 'collab-docs-mode)
         (collab--docs-refresh)))
  (message "Refreshed"))

(defun collab-list-docs ()
  "Show all connected and hosted files."
  (interactive)
  (switch-to-buffer (collab--docs-buffer))
  (collab-docs-mode)
  (collab--docs-refresh))
;;; Find-file

(defun collab--parse-filename (filename)
  "Parse FILENAME into FILE-DESC.

Return nil if parse failed."
  (let ((segments (string-split filename "/" t (rx (+ whitespace)))))
    (when (>= (length segments) 2)
      (let ((host-id (pop segments))
            (project (pop segments)))
        (if segments
            (collab--make-file-desc host-id project (string-join segments "/"))
          (collab--make-file-desc host-id project ""))))))

(defun collab--file-local-p (file-desc)
  "Return non-nil if FILE-DESC is a local file."
  (equal (plist-get file-desc :hostId) collab--my-host-id))

(defun collab--project-path (project)
  "Return the absolute path of PROJECT. If doesn’t exist, return nil.

If can’t fetch data from server, also return nil."
  (let* ((state (plist-get
                 (collab--use-query 'ConnectionState
                                    (lambda ()
                                      (collab--connection-state-req))
                                    30)
                 :data))
         (projects (plist-get state :projects))
         (project (seq-find (lambda (proj-data)
                              (equal (plist-get proj-data :name)
                                     project))
                            projects))
         (project-path (and project (plist-get project :path))))
    project-path))

(defun collab--local-filename (file-desc)
  "For local FILE-DESC, return the local full filename.
Return nil if FILE-DESC isn’t a local file."
  (when (collab--file-local-p file-desc)
    (let* ((project (plist-get file-desc :project))
           (project-path (collab--project-path project)))
      (expand-file-name (string-trim (plist-get file-desc :file) "/")
                        project-path))))

(defun collab--encode-filename (file-desc)
  "Encode FILE-DESC into a path.

Returned path doesn't have trailing slash even if it's a directory.
The host-id in the path is truncated for display but preserved in full."
  (let ((host-id (collab--prettify-host-id
                  (plist-get file-desc :hostId)))
        (project (plist-get file-desc :project))
        (file (plist-get file-desc :file)))
    (if (equal file "")
        (format "/%s/%s" host-id project)
      (format "/%s/%s/%s" host-id project file))))

(defun collab--file-name-directory (path)
  "Like ‘file-name-directory’ but for a collab PATH.

Bypasses ‘file-name-handler-alist’ because the host-id contains
“::”, which Tramp interprets as method-name."
  (let ((slash (cl-position ?/ path :from-end t)))
    (and slash (substring path 0 (1+ slash)))))

(defun collab--file-name-nondirectory (path)
  "Like ‘file-name-nondirectory’ but for a collab PATH.

Bypasses ‘file-name-handler-alist’ because the host-id contains
“::”, which Tramp interprets as method-name."
  (let ((slash (cl-position ?/ path :from-end t)))
    (if slash (substring path (1+ slash)) path)))

(defun collab--backup-filename (file-desc)
  "Return the backup file path of FILE-DESC."
  (expand-file-name (string-trim (collab--encode-filename file-desc) "/")
                    collab-backup-directory))

(defun collab--encode-parent (file-desc)
  "Encode FILE-DESC into a path of it's parent directory.

Returned path doesn't have trailing slash even if it's a directory."
  (when-let* ((parent (collab--file-desc-parent file-desc)))
    (collab--encode-filename parent)))

(defun collab--complete-filename-1 (segments dir-p)
  "Return the next segment completion for SEGMENTS.

SEGMENTS is a list of path segments. DIR-P is t if SEGMENTS is meant to
be a directory (the original path string ends with slash)."
  (let ((append-slash (lambda (str) (concat str "/")))
        (conn-state (plist-get
                     (collab--use-query 'ConnectionState
                                        #'collab--connection-state-req
                                        30)
                     :data)))
    (when (not conn-state)
      (user-error "Not connected to collab process"))
    (if (eq (length segments) 0)
        (seq-map append-slash
                 (seq-map #'collab--prettify-host-id
                          (collab--all-hosts conn-state)))
      (condition-case nil
          (let* ((host (car segments))
                 (file-desc (pcase (length segments)
                              (1 nil)
                              (2 (collab--make-file-desc
                                  host (nth 1 segments) ""))
                              (_ (collab--make-file-desc
                                  host
                                  (nth 1 segments)
                                  (string-join
                                   (seq-subseq segments 2 (if dir-p nil -1))
                                   "/")))))
                 (resp (collab--list-files-req file-desc host))
                 (files (plist-get resp :files)))
            (seq-map (lambda (file)
                       (let ((name (plist-get file :filename))
                             (dir-p (not (eq (plist-get file :isDirectory)
                                             :json-false))))
                         (concat name (if dir-p "/" ""))))
                     files))
        (error nil)))))

(defun collab--complete-filename (filename _pred flag)
  "Complete FILENAME.
PRED and FLAG see manual."
  (let* ((dir-p (string-match-p (rx "/" eos) filename))
         (segments (string-split filename "/" t (rx (+ whitespace))))
         (next (collab--complete-filename-1 segments dir-p)))
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


(defun collab--open-thing-at-point ()
  "Open the file at point.
There should be three text properties at point:

- ‘collab-file-desc’
- ‘collab-filename’
- ‘collab-directory-p’"
  (interactive)
  (let ((file-desc (get-text-property (point) 'collab-file-desc))
        (filename (get-text-property (point) 'collab-filename))
        (directory-p (get-text-property (point) 'collab-directory-p)))
    (collab--open-thing file-desc filename directory-p)))

(defun collab--open-thing (file-desc filename directory-p)
  "Open FILE-DESC.

FILENAME is the nondirectory filename for FILE-DESC. DIRECTORY-P is as
the name suggests.

If BUFFER non-nil, use the provided buffer rather than creating one."
  (interactive)
  (let* ((file-key (collab--encode-filename file-desc))
         (buf (gethash file-key collab--buffer-table)))
    (cond
     ((not (and file-desc filename))
      (user-error "Can’t find file at point"))
     ;; If the file is already opened and connected, just show it.
     ((and (buffer-live-p buf)
           (buffer-local-value 'collab-monitored-mode buf))
      (display-buffer buf))
     ;; Open a local file/directory
     ((collab--file-local-p file-desc)
      ;; This will run the ‘collab--find-file-hook’ which sets up the
      ;; collab buffer if it’s a file.
      (find-file (collab--local-filename file-desc)))
     ;; Open a remote directory or project.
     (directory-p
      (let* ((path (collab--encode-filename file-desc))
             (buf (get-buffer-create (format "%s<collab>" path)))
             (backup-path (collab--backup-filename file-desc)))
        (select-window (display-buffer buf))
        (when (not (file-exists-p backup-path))
          (mkdir backup-path t))
        (collab-dired-mode)
        (puthash file-key buf collab--buffer-table)
        (setq collab--file file-desc
              collab--default-directory path
              default-directory backup-path)
        (collab--dired-refresh)))
     ;; Open a remote file.
     (t
      (select-window
       (display-buffer
        (if (and buf (buffer-live-p buf))
            buf
          (generate-new-buffer (format "%s<collab>" filename)))))

      (collab--setup-file file-desc)

      (let ((backup-path (collab--backup-filename file-desc)))
        (setq default-directory (file-name-directory backup-path)
              buffer-file-name backup-path)
        (when (not (file-exists-p default-directory))
          (mkdir default-directory t))
        (write-file backup-path))))))

(defun collab--disconnect-from-doc ()
  "Disconnect from the file at point."
  (interactive)
  (let ((file-desc (get-text-property (point) 'collab-file-desc)))
    (when file-desc
      (collab--catch-error
          (format "can't disconnect from %s"
                  (collab--encode-filename file-desc))
        (when (jsonrpc-running-p collab--jsonrpc-connection)
          (collab--disconnect-from-file-req file-desc))))
    (let* ((file-key (collab--encode-filename file-desc))
           (buf (when file-desc
                  (gethash file-key collab--buffer-table))))
      (when buf
        (with-current-buffer buf
          (collab--disable)))
      (puthash file-key nil collab--buffer-table)))
  (collab--refresh))

(defun collab--close-doc ()
  "Close the file at point."
  (interactive)
  (let ((file-desc (get-text-property (point) 'collab-file-desc)))
    (when file-desc
      (collab--catch-error
          (format "can't close %s"
                  (collab--encode-filename file-desc))
        (collab--close-file-req file-desc))))
  (collab--refresh))

(defun collab--disconnect-or-close ()
  "Disconnect or close the file at point.
Disconnect for remote doc, close for owned doc."
  (interactive)
  (if (get-text-property (point) 'collab-hosted-file-p)
      (collab--close-doc)
    (collab--disconnect-from-doc)))

;; FIXME
(defun collab--delete-doc ()
  "Delete the file at point."
  (interactive)
  (when-let* ((file-desc (get-text-property (point) 'collab-file-desc)))
    (collab--catch-error (format "can't delete %s" (collab--encode-filename file-desc))
      (collab--delete-file-req file-desc))
    (collab--refresh)))

(defun collab--toggle-accept-connection ()
  "Toggle accepting remote connections.

If currently accepting, stop.  Otherwise start accepting on the
signaling address from ‘collab-default-signaling-server’."
  (interactive)
  (let* ((signaling-addr collab-default-signaling-server)
         (state (collab--connection-state-req))
         (accepting (plist-get state :accepting))
         (currently-accepting-p (> (seq-length accepting) 0)))
    (collab--catch-error "can’t toggle accept connection "
      (cond
       (currently-accepting-p
        (collab--stop-accepting-notif signaling-addr)
        (collab--msg-event 'success
                           "Stopping accepting remote connections"))
       (t
        (collab--accept-connection-notif signaling-addr)
        (collab--msg-event 'success "Attempting to start accepting"))))
    (collab--refetch 'ConnectionState
                     (lambda () (collab--connection-state-req)))
    (collab--hub-rerender)))

;;;###autoload
(defun collab-find-file ()
  "Find and open a collab file with auto-completion.
Uses ‘collab--default-directory’ as initial input."
  (interactive)
  (let ((path (completing-read
               "Find collab file: "
               #'collab--complete-filename
               nil nil (if collab--default-directory
                           (concat (string-trim-right
                                    collab--default-directory "/")
                                   "/")
                         "/"))))
    (if (not (and path (not (string-empty-p path))))
        (user-error "Path is empty")
      (let ((parsed (collab--parse-filename path)))
        (if (not parsed)
            (message "Invalid path format: %s" path)
          (collab--open-thing parsed
                              (collab--file-name-nondirectory path)
                              nil))))))

(defun collab-find-file-bifurcated ()
  "Find and open a collab file.

Use ‘find-file’ for local files. Use ‘collab-find-file’ for remote
files. Uses ‘collab--default-directory’ as initial input."
  (interactive)
  (let ((file-desc (and collab--default-directory
                        (collab--parse-filename
                         collab--default-directory))))
    (cond
     ((and file-desc (collab--file-local-p file-desc))
      (call-interactively #'find-file))
     (t
      (call-interactively #'collab-find-file)))))

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
    (collab--accept-connection-notif collab-default-signaling-server)
    (let* ((link (format "%s%s" collab-default-signaling-server
                         (collab--encode-filename file-desc))))
      (puthash 'CurrentMessage
               (collab--fairy "The file is shared and here’s the link
Your friends can connect with just a click!
LINK: %s" (propertize link 'face 'link))
               collab--cached-responses)
      (collab--hub-rerender))))

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
    (let* ((resp (collab--share-file-req
                  filename `( :common.hostEditor "emacs"
                              :emacs.majorMode
                              ,(symbol-name major-mode))
                  (buffer-substring-no-properties
                   (point-min) (point-max))))
           (file-desc (plist-get resp :file))
           (site-id (plist-get resp :siteId)))
      (collab--enable file-desc site-id
                      (collab--encode-filename file-desc))
      (display-buffer (collab--hub-buffer)
                      '(() . ((inhibit-same-window . t))))
      (collab--notify-newly-shared-doc file-desc))))

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
  (collab--catch-error "can't share project"
    (collab--declare-project-req project-name path)
    (collab--notify-newly-shared-doc
     (collab--make-file-desc collab--my-host-id project-name ""))
    ;; Refetch shared projects data.
    (collab--invalidate-query 'ConnectionState)))

(defalias 'collab-reconnect 'collab-resume)
(defun collab-resume (file-desc)
  "Reconnect current buffer to a remote document FILE-DESC.

Reconnecting will override the current content of the buffer.

If called interactively, uses the current buffer's FILE-DESC."
  (interactive (list collab--file))
  (collab--catch-error (format "can't reconnect to %s"
                               (collab--encode-filename file-desc))
    (let ((orig-point (point)))
      ;; Replace buffer content with document content.
      (let* ((resp (collab--open-file-req file-desc "open"))
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
        (collab--enable file-desc site-id
                        (collab--encode-filename file-desc))))))

(defun collab-disconnect-buffer ()
  "Disconnect current buffer, returning it to a regular buffer."
  (interactive)
  (collab--check-precondition)
  (collab--catch-error-no-signal
      (format "can't disconnect from %s" (collab--encode-filename collab--file))
    (collab--disconnect-from-file-req collab--file))
  (collab--disable)
  (message "Disconnected"))

(defun collab-share-link ()
  "Copy the share link of current doc to clipboard."
  (interactive)
  (let ((file-desc
         (if (derived-mode-p 'collab-hub-mode)
             (get-text-property (point) 'collab-file-desc)
           collab--file)))
    (if (not file-desc)
        (message (collab--fairy "Uhmmm, I can’t find the doc id of this doc..."))
      (let ((link (format "%s/%s"
                          collab-default-signaling-server
                          (collab--encode-filename file-desc))))
        (kill-new link)
        (message (collab--fairy "Copied link to clipboard for ya (%s)" link))))))

;;;###autoload
(defun collab-connect-remote ()
  "Connect to a known remote or with a share link.

Prompt for a remote host to connect to. Select the sepcial “By share
link” option to connect by a share link."
  (interactive)
  (let* ((by-link "By share link")
         (hosts (mapcar #'car collab-known-hosts))
         (choices (cons by-link hosts))
         (choice (completing-read "Connect to: " choices nil t))
         (link (if (equal choice by-link)
                   (read-string "Share link: ")
                 (let* ((data (cdr (assoc choice collab-known-hosts)))
                        (signaling (plist-get (plist-get data :SCTP)
                                              :signalingAddr)))
                   (format "%s/%s" signaling choice)))))
    (collab-connect link)))

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

    (let ((entry (cons host-id
                       (list :SCTP
                             (list :signalingAddr signaling-addr)))))
      (unless (alist-get host-id collab-known-hosts nil nil #'equal)
        (push entry collab-known-hosts))
      (unless (alist-get host-id collab--hosts-to-connect-to
                         nil nil #'equal)
        (push entry collab--hosts-to-connect-to)))

    (if (equal host-id collab--my-host-id)
        ;; TODO fairy.
        (message "You shouldn’t need to connect to a local doc")
      (switch-to-buffer (collab--hub-buffer))
      (collab-hub-mode)
      (when project
        (setq collab--open-this-doc
              (list host-id
                    (collab--make-file-desc host-id project (or path ""))
                    (if path (file-name-nondirectory path) project)
                    dir-p)))
      (collab--hub-refresh))))

(defun collab--print-history (&optional debug)
  "Print debugging history for the current buffer.
If called with an interactive argument (DEBUG), print more
detailed history."
  (interactive "p")
  (collab--check-precondition)
  (let ((debug (eq debug 4)))
    (collab--catch-error
        (format "can't print history of %s"
                (collab--encode-filename collab--file))
      (collab--send-ops-now)
      (let ((text (plist-get (collab--print-history-req collab--file debug)
                             :history))
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
  (unless collab-display-name
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
  (let ((display-name (read-string (collab--fairy "Heya! I'm dolly dolly dolly, the collab-mode fairy. Sweet human, tell me, what name do you carry? -- ") user-full-name)))
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

;;; Config editing

(defvar collab-config-mode-syntax-table
  (let ((st (make-syntax-table)))
    (modify-syntax-entry ?\; "<" st)
    (modify-syntax-entry ?\n ">" st)
    st)
  "Syntax table for `collab-config-mode'.")

(defvar collab-config-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "C-c C-c") #'collab-config-submit)
    (define-key map (kbd "C-c C-k") #'collab-config-abort)
    map)
  "Keymap for `collab-config-mode'.")

(defvar collab-config-mode-font-lock-keywords
  '((";.*" . font-lock-comment-face)
    ("\\[\\([WCDwcd]*\\)\\]" 1 font-lock-keyword-face)
    ("(\\([^)]+\\))" 1 font-lock-string-face))
  "Font-lock keywords for `collab-config-mode'.")

(define-derived-mode collab-config-mode fundamental-mode "Collab Config"
  "Mode for editing collab trusted hosts and permissions."
  :syntax-table collab-config-mode-syntax-table
  (setq-local comment-start ";; ")
  (setq-local font-lock-defaults '(collab-config-mode-font-lock-keywords)))

(defun collab--permission-to-string (perm)
  "Convert PERM plist to string like \"WCD\"."
  (concat (if (eq (plist-get perm :write) t) "W" "")
          (if (eq (plist-get perm :create) t) "C" "")
          (if (eq (plist-get perm :delete) t) "D" "")))

(defun collab--string-to-permission (str)
  "Convert permission STR like \"WCD\" to hash table for JSON serialization."
  (let ((ht (make-hash-table :test #'equal)))
    (puthash "write" (if (string-match-p "W" str) t :json-false) ht)
    (puthash "create" (if (string-match-p "C" str) t :json-false) ht)
    (puthash "delete" (if (string-match-p "D" str) t :json-false) ht)
    ht))

(defun collab-config ()
  "Open buffer for editing trusted hosts and permissions."
  (interactive)
  (let ((buf (get-buffer-create "*collab trusted hosts*")))
    (pop-to-buffer buf)
    (let ((inhibit-read-only t))
      (erase-buffer)
      (collab-config-mode)
      ;; Insert instructions.
      (insert (substitute-command-keys
               ";; Edit trusted hosts and their permission in this buffer, and
;; type \\[collab-config-submit] to submit, or type \\[collab-config-abort] to cancel
;; Each line is made of <host-id> [ <permission> ]
;; <permission> can be any combination of `W', `C', and `D'
;; `W': write, `C': create, `D': delete, empty means read-only.\n\n"))
      ;; Fetch current config.
      (condition-case err
          (let* ((resp (collab--connection-state-req))
                 (trusted-hosts (plist-get resp :trustedHosts))
                 (permissions (plist-get resp :permission)))
            ;; Insert each trusted host.
            (mapc (lambda (host-id)
                    (let* ((key (intern (concat ":" host-id)))
                           (perm (plist-get permissions key))
                           (perm-str (if perm
                                         (collab--permission-to-string perm)
                                       "")))
                      (insert (format "%s[%s]\n"
                                      (collab--prettify-host-id host-id) perm-str))))
                  trusted-hosts))
        (error
         (insert (format ";; Error fetching currently trusted hosts: %s\n" err)))))))

(defun collab-config-abort ()
  "Abort editing and kill the config buffer."
  (interactive)
  (kill-buffer))

(defun collab-config-submit ()
  "Parse the buffer and submit the config update."
  (interactive)
  (let ((trusted-hosts nil)
        (permissions (make-hash-table :test #'equal))
        (errors nil)
        (inhibit-read-only t))
    ;; Remove any previous error overlays.
    (remove-overlays (point-min) (point-max) 'collab-config-error t)
    ;; Parse each line.
    (save-excursion
      (goto-char (point-min))
      (while (not (eobp))
        (let ((line (buffer-substring-no-properties (pos-bol) (pos-eol))))
          (cond
           ;; Skip empty lines and comments.
           ((string-match-p (rx bos (* space) eos) line))
           ((string-match-p (rx bos (* space) ";") line))
           ;; Parse data line: <host-id>[<permission>]
           ((string-match (rx bos (* space)
                              (group (+? nonl))
                              (* space) "[" (* space)
                              (group (* (any "WCDwcd")))
                              (* space) "]"
                              (* space) eos)
                          line)
            (let* ((host-id (string-trim (match-string 1 line)))
                   (perm-str (upcase (match-string 2 line))))
              (push host-id trusted-hosts)
              (puthash host-id (collab--string-to-permission perm-str) permissions)))
           ;; Invalid line.
           (t
            (push (point) errors)
            (let ((ov (make-overlay (pos-eol) (pos-eol))))
              (overlay-put ov 'after-string
                           (propertize " <-- parse error: expected <host-id>[<permission>]"
                                       'face 'error))
              (overlay-put ov 'collab-config-error t)))))
        (forward-line 1)))
    ;; If errors, don't submit.
    (when (not errors)
      ;; Submit the config.
      (condition-case err
          (progn
            (collab--update-config-req trusted-hosts permissions)
            (kill-buffer)
            (message (collab--fairy "Config updated!")))
        (error
         (message "Failed to update config: %s" err))))))

(provide 'collab-mode)

;;; collab-mode.el ends here
