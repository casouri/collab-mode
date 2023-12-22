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

(defgroup collab
  ()
  "Collaboration mode."
  :group 'editting)

(defcustom collab-display-name nil
  "The display name of the user."
  :type 'string)

(defcustom collab-local-server-config nil
  "A list storing configuration for the local server.
The list should be (HOST-ID SIGNALING-SERVER-ADDR)."
  :type '(list string))

(defface collab-success-background
  '((t . (:inherit diff-refine-added :extend t)))
  "The highlight used for indicating a success.")

(defface collab-failure-background
  '((t . (:inherit diff-refine-removed :extend t)))
  "The highlight used for indicating a failure.")

(defvar collab-rpc-timeout 1
  "Timeout in seconds to wait for connection to collab process.")

(defvar collab-command "~/p/collab/target/debug/collab"
  "Path to the collab server executable.")

(defvar collab-connection-type '(socket 7701)
  "How to connect to the collab process.
The value can be ‘pipe’, meaning use stdio for connection,
or (socket PORT), meaning using a socket connection, and PORT
should be the port number.")

(defvar collab-server-alist nil
  "An alist of server configurations.
Each key is HOST-ID, each value is (SIGNALING-SERVER-ADDR
CREDENTIAL).")

(defvar collab-hasty-p t
  "If non-nil, buffer changes are sent to collab process immediately.")

;; Generated with seaborn.color_palette("colorblind").as_hex()
(defvar collab-cursor-colors
  '( "#0173b2" "#de8f05" "#029e73" "#d55e00" "#cc78bc" "#ca9161"
     "#fbafe4" "#949494" "#ece133" "#56b4e9")
  "Cursor color ring.")

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

    (100 . NetworkError)
    (101 . ServerNonFatalDocFatal)
    (102 . ServerFatalError)

    (104 . PermissionDenied)
    (105 . DocNotFound)
    (106 . DocAlreadyExists)
    (107 . IOError)
    (108 . NotRegularFile)
    (109 . UnsupportedOperation))
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
(defvar collab--doc-and-host)
(defun collab--check-precondition ()
  "Check ‘collab--jsonrpc-connection’ and ‘collab--doc-and-host’.

If they are invalid, turn off ‘collab-monitored-mode’ and raise
an error. This function should be called before any interactive
command that acts on a collab-monitored buffer."
  (unless collab--jsonrpc-connection
    (collab-monitored-mode -1)
    (display-warning 'collab "Connection to collab server broke"))
  (unless collab--doc-and-host
    (collab-monitored-mode -1)
    (display-warning
     'collab "Current buffer doesn’t have a doc id or server id")))


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
        (overlay-put ov 'after-string nil)))))

(defvar collab-monitored-mode)
(defun collab--send-cursor-pos-1 (buffer)
  "Send cursor position of BUFFER to the collab process."
  (when (equal buffer (current-buffer))
    (collab--check-precondition)
    (collab--catch-error "can’t send cursor position to remote"
      (let* ((doc-id (car collab--doc-and-host))
             (host-id (cdr collab--doc-and-host))
             (site-id collab--my-site-id))
        (collab--send-info-req
         doc-id host-id
         (if (region-active-p)
             `( :type "reserved.pos" :point ,(point)
                :mark ,(mark) :siteId ,site-id)
           `(:type "reserved.pos" :point ,(point) :siteId ,site-id)))))

    (remhash collab--doc-and-host
             collab--sync-cursor-timer-table)))

(defun collab--send-cursor-pos ()
  "Move user (SITE-ID)’s cursor overlay to POS."
  (collab--check-precondition)
  (when (null (gethash collab--doc-and-host
                       collab--sync-cursor-timer-table))
    ;; Run with an idle timer to we don’t interrupt scrolling, etc.
    (let ((timer (run-with-idle-timer
                  0.5 nil #'collab--send-cursor-pos-1
                  (current-buffer))))
      (puthash collab--doc-and-host timer
               collab--sync-cursor-timer-table))))


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
;; Amalgamation is done in post-command-hook. If the current command
;; only created one op (usually inserting or deleting one character),
;; we try to amalgamate it with the last op.
;;
;; Because future op might be able to amalgamate with the current op,
;; we delay sending ops to the collab process until a) user created an
;; op that can’t amalgamate with the pending ops, in which case we
;; send the pending ops and keep the new op to be amalgamated in the
;; future; or b) 1s passed, in which case we send the op regardless.
;; Case b) uses an idle timer.

(defvar-local collab--doc-and-host nil
  "(DOC-ID . HOST-ID) for the current buffer.
For ‘collab-dired-mode’ buffers, it’s (DOC-DESC . HOST-ID.")

(defvar-local collab--accepting-connection nil
  "If non-nil, our collab process is accepting remote connections.")

(defvar-local collab--most-recent-error nil
  "Most recent error message.")

(defvar collab--buffer-table (make-hash-table :test #'equal)
  "A has table that maps (DOC . SERVER) to their corresponding buffer.")

(defvar-local collab--inhibit-hooks nil
  "When non-nil, post command hooks don’t run.")

(defvar-local collab--changing-region nil
  "Records information of the region to be changed.
The shape is (beg end content), where CONTENT is the buffer
content between BEG and END.")

(defvar-local collab--pending-ops nil
  "Ops waiting to be sent out. In reverse order.")

(defvar-local collab--ops-current-command nil
  "Ops created in the current command. In reverse order.")

(defvar-local collab--group-seq nil
  "Group sequence number.
Group sequence is used to mark undo groups. consecutive ops with
the same group sequence are undone together.")

(defvar collab--edit-tracking-timer nil
  "An idle timer that sends pending ops to collab process.")

(defvar collab--idle-timer-registered-buffer nil
  "If a buffer has pending ops, it adds itself to this list.
So that the idle timer can send its pending ops.")

(defun collab--server-alist ()
  "Return ‘collab--server-alist’ plus “self”."
  (cons '("self" . ("" ""))
        collab-server-alist))

(defun collab--before-change (beg end)
  "Records the region (BEG . END) to be changed."
  (when (not (eq beg end))
    (setq collab--changing-region
          (list beg end (buffer-substring-no-properties beg end)))))

(defun collab--after-change (beg end len)
  "Record the changed region (BEG END LEN)."
  (let ((error-fn (lambda ()
                    (display-warning
                     'collab
                     "Buffer change not correctly recorded (exceeds recorded region: %s)"
                     collab--changing-region)
                    (collab-monitored-mode -1)
                    (throw 'term nil)))
        (group-seq collab--group-seq)
        ops)
    (catch 'term
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
                     (push (list 'del beg old group-seq) ops)
                     (push (list 'ins beg current group-seq) ops))))

             (funcall error-fn)))
          (_ (funcall error-fn))))

      (setq collab--changing-region nil)
      (setq collab--ops-current-command
            (nconc ops collab--ops-current-command)))))

(defun collab--pre-command ()
  "Increment ‘collab--group-seq’."
  (when (numberp collab--group-seq)
    (cl-incf collab--group-seq)))

(defun collab--post-command-hasty ()
  "Like ‘collab--post-command’ but just send ops out."
  (when (not collab--inhibit-hooks)
    (if (not collab--ops-current-command)
        (collab--send-cursor-pos)
      (collab--send-ops (nreverse collab--ops-current-command))
      (setq collab--ops-current-command nil))))

(defun collab--post-command ()
  "Convert ops in the buffer and send them to the collab process.
Then apply the returned remote ops."
  (when (not collab--inhibit-hooks)
    (if (not collab--ops-current-command)
        (collab--send-cursor-pos)
      (if-let ((amalgamated-op
                (and (eq 1 (length collab--ops-current-command))
                     (let ((this-op
                            (car collab--ops-current-command))
                           (last-op (car collab--pending-ops)))
                       (and last-op
                            (collab--maybe-amalgamate
                             last-op this-op))))))
          ;; Amalgamated, don’t send ops yet.
          (progn
            (setcar collab--pending-ops amalgamated-op)
            (message "Amalgamate, %s" collab--pending-ops))
        ;; Didn’t amalgamate, send ops.
        (message "No amalgamate, %s %s"
                 collab--pending-ops
                 collab--ops-current-command)

        (collab--send-ops (nreverse collab--pending-ops))
        (setq collab--pending-ops collab--ops-current-command))

      (setq collab--ops-current-command nil)

      ;; Register this buffer so the idle timer will send pending ops.
      (when (not (memq (current-buffer)
                       collab--idle-timer-registered-buffer))
        (push (current-buffer)
              collab--idle-timer-registered-buffer)))))

(defun collab--send-ops-now ()
  "Immediately send any pending ops to the collab process."
  (cl-assert (null collab--ops-current-command))
  (message "Sending pending ops: %s" collab--pending-ops)
  (let ((ops (nreverse collab--pending-ops)))
    (collab--send-ops ops)
    (setq collab--pending-ops nil)))

(defun collab--timer-send-ops ()
  "Send all pending ops to collab process."
  (dolist (buffer collab--idle-timer-registered-buffer)
    (with-current-buffer buffer
      (collab--send-ops-now)))
  (setq collab--idle-timer-registered-buffer nil))

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
       nil))))

(defvar collab--mode-line
  `(collab-monitored-mode
    ,(propertize "CONNECTED " 'face
                 '(:inherit (bold success))))
  "Mode-line indicator for ‘collab-monitored-mode’.")

(defvar collab-monitored-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map [remap undo-only] #'collab-undo)
    (define-key map [remap undo-redo] #'collab-redo)
    (define-key map (kbd "C-/") #'collab-undo)
    (define-key map (kbd "C-.") #'collab-redo)
    (define-key map [remap undo]
                (lambda (&rest _)
                  (interactive)
                  (message (collab--fairy "Emacs-undo won’t work in this mode, use ‘collab-undo/redo’ is what I was told! (undo → \\[collab-undo], redo → \\[collab-redo])"))))
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
        (add-hook 'pre-command-hook #'collab--pre-command 0 t)
        (if collab-hasty-p
            (add-hook 'post-command-hook
                      #'collab--post-command-hasty 0 t)
          (add-hook 'post-command-hook #'collab--post-command 0 t))

        (unless collab--edit-tracking-timer
          (setq collab--edit-tracking-timer
                (run-with-idle-timer 1 t #'collab--timer-send-ops)))

        (unless (member collab--mode-line mode-line-misc-info)
          (setq-local mode-line-misc-info
                      (cons collab--mode-line mode-line-misc-info)))

        ;; (setq-local buffer-undo-list t)

        (setq-local collab--pending-ops nil)
        (setq-local collab--ops-current-command nil)
        (setq-local collab--group-seq 1))

    ;; Disable.
    (remhash collab--doc-and-host
             collab--dispatcher-timer-table)

    (remove-hook 'before-change-functions #'collab--before-change t)
    (remove-hook 'after-change-functions #'collab--after-change t)
    (remove-hook 'pre-command-hook #'collab--pre-command t)
    (remove-hook 'post-command-hook #'collab--post-command t)
    (remove-hook 'post-command-hook #'collab--post-command-hasty t)

    (remhash collab--doc-and-host
             collab--buffer-table)

    (dolist (ov collab--cursor-ov-alist)
      (delete-overlay (cdr ov)))
    (setq-local collab--cursor-ov-alist nil)

    ;; (setq-local buffer-undo-list nil)
    (setq-local collab--doc-and-host nil)
    (setq-local collab--my-site-id nil)))

(defun collab--enable (doc-id host-id my-site-id)
  "Enable ‘collab-monitored-mode’ in the current buffer.
DOC-ID and HOST-ID are associated with the current buffer.
MY-SITE-ID is the site if of this editor."
  (setq collab--doc-and-host (cons doc-id host-id))
  (setq collab--my-site-id my-site-id)
  (puthash collab--doc-and-host
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

(defvar collab--stashed-state-plist nil
  "Used for stashing local state when major mode changes.")

(defun collab--rescue-state ()
  "Stash local variable elsewhere before major mode changes."
  (setq collab--stashed-state-plist
        (plist-put collab--stashed-state-plist
                   (current-buffer)
                   `( :doc-server ,collab--doc-and-host
                      :site-id ,collab--my-site-id))))

(defun collab--maybe-recover ()
  "Maybe reenable ‘collab’ after major mode change."
  (when-let* ((state (plist-get collab--stashed-state-plist
                                (current-buffer)))
              (doc-server (plist-get state :doc-server))
              (site-id (plist-get state :site-id)))
    (collab--enable (car doc-server) (cdr doc-server) site-id)))

(defun collab--doc-connected (doc server)
  "Return non-nil if DOC at SERVER is connected."
  (let ((buf (gethash (cons doc server)
                      collab--buffer-table)))
    (and buf (buffer-live-p buf))))

;;; JSON-RPC

(defun collab--doc-desc-id (doc-desc)
  "Return the doc-id in DOC-DESC if it has one, else return nil.

DOC-DESC := (:Doc DOC-ID)
         | (:File [DOC-ID REL-PATH])
         | (:Dir [DOC-ID REL-PATH])
"
  (and (eq (car doc-desc) :Doc) (cadr doc-desc)))

(defun collab--doc-desc-dir-p (doc-desc)
  "Return non-nil if DOC-DESC represents a directory."
  (eq (car doc-desc) :Dir))

(defun collab--doc-desc-path (doc-desc)
  "Return the relative path in DOC-DESC."
  (pcase doc-desc
    (`(:File [,_ ,path]) path)
    (`(:Dir [,_ ,path]) path)
    (_ nil)))

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

(defvar collab--jsonrpc-connection nil
  "The JSONRPC connection to the collab server.")

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
                          :command (list collab-command)
                          :connection-type 'pipe)))))

        (let ((conn (make-instance 'jsonrpc-process-connection
                                   :name "collab"
                                   :process process
                                   :notification-dispatcher
                                   #'collab--dispatch-notification)))

          (when collab--jsonrpc-connection
            (jsonrpc-shutdown collab--jsonrpc-connection))
          (setq collab--jsonrpc-connection conn)))))

;;;; Dispatcher

(defvar collab--dispatcher-timer-table
  (make-hash-table :test #'equal)
  "Hash table mapping (doc-id . host-id) to dispatcher timers.")


(defun collab--request-remote-ops-before-seq (doc server buffer seq)
  "Request for remote ops for (DOC SERVER) in BUFFER.
Keep requesting for ops until we get all the ops before and
including SEQ."
  (let ((received-largest-seq nil))
    (unwind-protect
        (with-current-buffer buffer
          (setq received-largest-seq (collab--send-ops nil)))
      (let ((timer (if (or (null received-largest-seq)
                           (>= received-largest-seq seq))
                       nil
                     ;; If we didn’t get enough ops that we expect,
                     ;; schedule another fetch.
                     (run-with-timer
                      0 nil
                      #'collab--request-remote-ops-before-seq
                      doc server buffer seq))))
        (puthash (cons doc server) timer
                 collab--dispatcher-timer-table)))))

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
           (server (plist-get params :hostId))
           (last-seq (plist-get params :lastSeq)))
       (let ((buffer (gethash (cons doc server)
                              collab--buffer-table))
             (timer (gethash (cons doc server)
                             collab--dispatcher-timer-table)))
         (when timer (cancel-timer timer))
         (when (buffer-live-p buffer)
           (setq timer (run-with-timer
                        0 nil
                        #'collab--request-remote-ops-before-seq
                        doc server buffer last-seq))
           (puthash (cons doc server) timer
                    collab--dispatcher-timer-table)))))
    ('Info
     (let* ((doc-id (plist-get params :docId))
            (host-id (plist-get params :hostId))
            (value (plist-get params :value)))
       (pcase (plist-get value :type)
         ("reserved.pos" (let* (
                                (pos (plist-get value :point))
                                (mark (plist-get value :mark))
                                (site-id (plist-get value :siteId))
                                (buf (gethash (cons doc-id host-id)
                                              collab--buffer-table)))
                           (when (buffer-live-p buf)
                             (with-current-buffer buf
                               (collab--move-cursor site-id pos mark)))))))
     )
    ('ServerError
     (display-warning
      'collab
      (format "Local server errored, you might want to restart collab process. Cause: %s"
              params)))
    ('SignalingTimesUp
     (run-with-idle-timer
      0 nil
      (lambda ()
        (with-current-buffer (collab--hub-buffer)
          (setq collab--accepting-connection nil)
          (collab--refresh)
          (message "Collab process stopped accepting connections")))))
    ('AcceptConnectionErr
     ;; If we use ‘run-with-timer’, this handler might interfer with
     ;; the condition-case handler in ‘collab--render’.
     (run-with-idle-timer
      0 nil
      (lambda ()
        (with-current-buffer (collab--hub-buffer)
          (setq collab--accepting-connection nil)
          (collab--hub-refresh)
          (display-warning
           'collab
           (format "Error accepting remote peer connections: %s" params))))))))

;;;; Requests

(defun collab--list-files-req (dir server signaling-server credential)
  "Return a list of files on SERVER (address) with CREDENTIAL.

If DIR is non-nil, it should be the DOC-DESC of the directory we
want to list. SIGNALING-SERVER is the address of the signaling
server. Each file is of the form (:docDesc DOC-DESC :fileName
FILE-NAME)."
  (let* ((conn (collab--connect-process))
         (resp (jsonrpc-request
                conn 'ListFiles
                (append `( :hostId ,server
                           :signalingAddr ,signaling-server
                           :credential ,credential)
                        (and dir `(:dir ,dir)))
                :timeout collab-rpc-timeout)))
    ;; Convert vector to a list.
    (seq-map #'identity (plist-get resp :files))))

(defun collab--share-file-req (server filename file-meta content)
  "Share the file with SERVER (server id).
FILENAME is filename, FILE-META is a plist-encoded JSON object,
CONTENT is just the content of the file in a string.
Return (:docId DOC-ID :siteId SITE-ID). If FORCE is non-nil,
override existing files."
  (let* ((conn (collab--connect-process))
         (resp (jsonrpc-request
                conn 'ShareFile
                `( :hostId ,server
                   :fileName ,filename
                   :fileMeta ,file-meta
                   :content ,content)
                :timeout collab-rpc-timeout)))
    resp))

(defun collab--share-dir-req (server dir-name path)
  "Share the directory with SERVER (address).
DIR-NAME is the given name for the directory, PATH is the
absolute path of the directory. Return (:docId DOC-ID). If FORCE
is non-nil, override existing directories."
  (let* ((conn (collab--connect-process))
         (resp (jsonrpc-request
                conn 'ShareDir
                `( :hostId ,server
                   :dirName ,dir-name
                   :path ,path)
                :timeout collab-rpc-timeout)))
    resp))

(defun collab--connect-to-file-req (doc-desc server)
  "Connect to DOC-DESC on SERVER (server address).
Return (:siteId SITE-ID :content CONTENT)."
  (let* ((conn (collab--connect-process))
         (resp (jsonrpc-request
                conn 'ConnectToFile
                `( :hostId ,server
                   :docDesc ,doc-desc)
                :timeout collab-rpc-timeout)))
    resp))

(defun collab--disconnect-from-file-req (doc server)
  "Disconnect from DOC on SERVER."
  (let ((conn (collab--connect-process)))
    (jsonrpc-request conn 'DisconnectFromFile
                     `( :hostId ,server
                        :docId ,doc)
                     :timeout collab-rpc-timeout)))

(defun collab--delete-file-req (doc server)
  "Delete DOC on SERVER."
  (let ((conn (collab--connect-process)))
    (jsonrpc-request conn 'DeleteFile
                     `( :hostId ,server
                        :docId ,doc)
                     :timeout collab-rpc-timeout)))

(defun collab--accept-connection-req (host-id signaling-addr)
  "Accept connections as HOST-ID on SIGNALING-ADDR."
  (let ((conn (collab--connect-process)))
    (jsonrpc-request conn 'AcceptConnection
                     `( :hostId ,host-id
                        :signalingAddr ,signaling-addr)
                     :timeout collab-rpc-timeout)))

(defun collab--print-history-req (doc server debug)
  "Print debugging history for (DOC SERVER).
If DEBUG non-nil, print debug version."
  (let ((conn (collab--connect-process)))
    (jsonrpc-request conn 'PrintHistory
                     `( :docId ,doc
                        :hostId ,server
                        :debug ,(if debug t :json-false))
                     :timeout collab-rpc-timeout)))

(defun collab--send-info-req (doc server info)
  "Send INFO to DOC on SERVER.
INFO should be a plist JSON object. This request is async."
  (let ((conn (collab--connect-process)))
    (jsonrpc-notify conn 'SendInfo
                    `( :hostId ,server
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
             (reverse edits))))
    (unless move-point
      (goto-char start))))

(defun collab--send-ops (ops &optional encoded)
  "Send OPS to the collab server and apply returned remote ops.

If ENCODED is non-nil, OPS should be already in sexp JSON
format (a list of EditorOp, probably the Undo or Redo variant).

Return the largest global seq of all the ops received from the
collab process."
  (collab--check-precondition)
  (let ((conn collab--jsonrpc-connection)
        resp)
    (collab--catch-error "can’t send local edits to remote"
      ;; RESP := (:op LEAN-OP :siteId SITE-ID)
      ;; LEAN-OP := (:Ins [POS STR]) | (:Del [[POS STR]])
      (setq resp
            (jsonrpc-request
             conn 'SendOp
             `( :docId ,(car collab--doc-and-host)
                :hostId ,(cdr collab--doc-and-host)
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
             (collab--move-cursor
              site-id (1+ (aref (aref edits (1- (length edits))) 0)))))
          (`(:op (:Del ,edits) :siteId ,site-id)
           (when (> (length edits) 0)
             (collab--move-cursor
              site-id (1+ (aref (aref edits 0) 0))))))
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
                             `( :hostId ,(cdr collab--doc-and-host)
                                :docId ,(car collab--doc-and-host)
                                :kind ,(if redo "Redo" "Undo"))
                             :timeout collab-rpc-timeout))
      (if-let ((instructions (plist-get resp :ops)))
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

;;; UI

(defun collab--hub-buffer ()
  "Return the hub buffer."
  (get-buffer-create "*collab*"))

(defvar collab--current-message nil
  "If non-nil, collab hub will show this message in the UI.
This should be let-bound when calling ‘collab-hub’, rather than
set globally.")

(defmacro collab--save-excursion (&rest body)
  "Save position, execute BODY, and restore point.
Point is not restored in the face of non-local exit."
  `(let ((pos (point-marker)))
     ,@body
     (goto-char pos)))

(defface collab-dynamic-highlight
  '((t . (:background "gray90" :extend t)))
  "Face used for the dynamic highlight in the collab buffer.")

(defface collab-server '((t . (:inherit bold)))
  "Face used for the server line in the collab buffer.")

(defface collab-file '((t . (:inherit default)))
  "Face used for files in the collab buffer.")

;;;; Drawing the hub UI

(defun collab--insert-file
    (doc-desc file-name server &optional display-name)
  "Insert DOC-DESC with FILE-NAME.

SERVER is its server id. If DISPLAY-NAME non-nil, use that as the
display name.

See ‘collab--doc-desc-id’ for the shape of DOC-DESC."
  (let ((beg (point)))
    (insert (or display-name file-name))
    (add-text-properties
     beg (point) `( collab-doc-desc ,doc-desc
                    collab-file-name ,file-name))
    ;; If the doc is connected, it must be a :Doc.
    (when (and (collab--doc-desc-id doc-desc)
               (collab--doc-connected
                (collab--doc-desc-id doc-desc) server))
      (insert " " (propertize (icon-string 'collab-status-on)
                              'collab-status t)))))

(defun collab--insert-server (server signaling-server credential)
  "Insert SERVER (server id) on SIGNALING-SERVER and its files.
Server has CREDENTIAL. SIGNALING-SERVER is the address of the
signaling server. Return t if the server has some file to list."
  (let ((beg (point)))
    ;; 1. Insert server line.
    (insert (propertize server 'face 'bold
                        'collab-server-line t))
    (unless (equal server "self")
      (insert (propertize " UP" 'face 'success)))
    (insert (propertize "\n" 'line-spacing 0.4))
    (let ((files (collab--list-files-req
                  nil server signaling-server credential)))
      ;; 2. Insert files.
      (if files
          ;; FILE is in DocInfo type.
          (dolist (file files)
            (let ((doc (plist-get file :docDesc))
                  (file-name (plist-get file :fileName)))
              (collab--insert-file doc file-name server)
              (insert "\n")))
        (insert (propertize "(empty)\n" 'face 'shadow)))
      ;; 4. Mark server section.
      (add-text-properties beg (point)
                           `(collab-host-id ,server))
      ;; Don’t show the local server if there’s no hosted files.
      (when (and (equal server "self") (null files))
        (delete-region beg (point)))
      (if files t nil))))

(defun collab--insert-disconnected-server
    (server &optional insert-status)
  "Insert a disconnected SERVER.
If INSERT-STATUS, insert a red DOWN symbol."
  (let ((beg (point)))
    (insert (propertize server 'face 'bold))
    (when insert-status
      (insert (propertize " DOWN" 'face 'error)))
    (insert (propertize "\n" 'line-spacing 0.4))
    (insert "...\n")
    (add-text-properties beg (point)
                         `(collab-host-id ,server))))

(defun collab--prop-section (prop)
  "Return the (BEG . END) of the range that has PROP."
  (save-excursion
    (let ((forward-prop (text-property-search-forward prop))
          (backward-prop (text-property-search-backward prop)))
      (cons (prop-match-beginning backward-prop)
            (prop-match-end forward-prop)))))

(defvar collab-hub-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "RET") #'collab--open-doc)
    (define-key map (kbd "x") #'collab--delete-doc)
    (define-key map (kbd "k") #'collab--disconnect-from-doc)
    (define-key map (kbd "g") #'collab--refresh)
    (define-key map (kbd "A") #'collab--accept-connection)
    (define-key map (kbd "C") #'collab-connect)

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

(defun collab--render ()
  "Render collab mode buffer.

Also insert ‘collab--current-message’ if it’s non-nil."
  (let ((connection-up nil)
        (have-some-file nil))
    ;; Insert headers.
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
      (insert (format "Server at: %s/%s"
                      (nth 1 collab-local-server-config)
                      (car collab-local-server-config))
              "\n"))
    ;; (insert "Errors: ")
    ;; (insert "0" "\n")
    (insert "\n")

    (when collab--current-message
      (let ((beg (point)))
        (insert "\n" collab--current-message "\n\n")
        (font-lock-append-text-property beg (point) 'face 'diff-added)
        (insert "\n")))

    ;; Insert each server and files.
    (dolist (entry (collab--server-alist))
      (let ((server-beg (point))
            (server-has-some-file nil))
        (condition-case err
            (progn
              (setq server-beg (point))
              (if (not connection-up)
                  (collab--insert-disconnected-server (car entry))
                (setq server-has-some-file
                      (collab--insert-server
                       (car entry) (nth 0 (cdr entry))
                       (nth 1 (cdr entry))))))
          (jsonrpc-error
           (delete-region server-beg (point))
           (collab--insert-disconnected-server (car entry) t)
           (setq collab--most-recent-error
                 (format "Error connecting to remote peer:\n%s"
                         (pp-to-string err)))))
        (setq have-some-file (or have-some-file server-has-some-file)))
      (insert "\n"))

    ;; Footer.
    (unless have-some-file
      (insert "\n" (collab--fairy "No shared docs, not here, not now.
Let’s create one, and here’s how!\n\n\n")))

    (insert (substitute-command-keys
             "PRESS + TO SHARE A FILE
PRESS \\[collab-connect] TO CONNECT TO A REMOTE DOC
PRESS \\[collab--accept-connection] TO ACCEPT REMOTE CONNECTIONS (for 180s)\n"))
    (when collab--most-recent-error
      (insert
       (propertize (concat "\n\nMost recent error:\n\n"
                           collab--most-recent-error
                           "\n")
                   'face 'shadow))
      (setq collab--most-recent-error nil))))

;;;; Dynamic highlight

(defvar-local collab--hl-ov-1 nil
  "Overlay used for 1st level highlight.")

(defvar-local collab--hl-ov-2 nil
  "Overlay used for 2nd level highlight.")

(defun collab--dynamic-highlight ()
  "Highlight server and file at point."
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
     ((get-text-property (point) 'collab-server-line)
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

(defun collab--eldoc (callback)
  "Displays help information at point.
Used for ‘eldoc-documentation-functions’, calls CALLBACK
immediately."
  (let ((doc-desc (get-text-property (point) 'collab-doc-desc))
        (host-id (get-text-property (point) 'collab-host-id)))
    (cond
     (doc-desc
      (funcall callback
               "RET → Open File  x → Delete File  k → Disconnect"))
     (host-id
      (funcall callback "s → Share File"))
     (t
      (funcall callback "g → Refresh")))))

;;;; Collab-dired

(defvar-local collab--dired-rel-path nil
  "Relative path of this directory in the top-level shared directory.")

(defvar-local collab--dired-root-name nil
  "Name of the top-level shared directory this directory is in.")

(defvar collab-dired-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "RET") #'collab--open-doc)
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
  (when (and (derived-mode-p 'collab-dired-mode) collab--doc-and-host)
    (cl-labels ((root-dir-p (path) (equal path ".")))
      (collab--catch-error (format "can’t connect to %s"
                                   (car collab--doc-and-host))
        (let* ((doc-desc (car collab--doc-and-host))
               (host-id (cdr collab--doc-and-host))
               (info (alist-get host-id (collab--server-alist)
                                nil nil #'equal))
               (file-list (collab--list-files-req
                           doc-desc host-id (nth 0 info) (nth 1 info)))
               (parent-doc-desc (collab--doc-desc-parent doc-desc)))
          (insert "Collab ( " collab--dired-root-name " / "
                  (if (root-dir-p collab--dired-rel-path)
                      ""
                    collab--dired-rel-path)
                  " ):\n")
          (insert ".\n")
          (if (root-dir-p collab--dired-rel-path)
              (insert "..")
            (collab--insert-file
             parent-doc-desc
             (string-trim-right
              (or (file-name-directory collab--dired-rel-path) ".") "/")
             host-id ".."))
          (insert "\n")
          (if (null file-list)
              (insert (collab--fairy "It’s so empty here!\n"))
            (dolist (file file-list)
              (let* ((doc (plist-get file :docDesc))
                     (file-name (plist-get file :fileName)))
                (collab--insert-file
                 doc (if (equal file-name "") "." file-name) host-id)
                (insert "\n"))))
          (put-text-property (point-min) (point-max)
                             'collab-host-id host-id))))))

;;; Interactive commands

(defun collab-shutdown ()
  "Shutdown the connection to the collab process."
  (interactive)
  (when collab--jsonrpc-connection
    (jsonrpc-shutdown collab--jsonrpc-connection)
    (setq collab--jsonrpc-connection nil)))

(defun collab--hub-refresh ()
  "Refresh collab hub buffer."
  (interactive)
  (let ((inhibit-read-only t)
        (line (line-number-at-pos)))
    (erase-buffer)
    (collab--render)
    (goto-char (point-min))
    (forward-line (1- line))))

(defun collab--refresh ()
  "Refresh current buffer, works in both hub and dired mode."
  (interactive)
  (cond ((derived-mode-p 'collab-hub-mode)
         (collab--hub-refresh))
        ((derived-mode-p 'collab-dired-mode)
         (collab--dired-refresh))))

(defun collab--open-doc (&optional doc-id host-id)
  "Open the file at point.
There should be three text properties at point:

- ‘collab-doc-desc’
- ‘collab-file-name’
- ‘collab-host-id’

If HOST-ID and DOC-ID non-nil, use them instead."
  (interactive)
  ;; See ‘collab--doc-desc-id’ for the shape of DOC-DESC.
  (let* ((doc-desc (or (and doc-id `(:Doc ,doc-id))
                       (get-text-property (point) 'collab-doc-desc)))
         (file-name (get-text-property (point) 'collab-file-name))
         (host-id (or host-id
                      (get-text-property (point) 'collab-host-id)))
         (buf (when-let ((doc-id (collab--doc-desc-id doc-desc)))
                (gethash (cons doc-id host-id)
                         collab--buffer-table))))
    (cond
     ((buffer-live-p buf)
      (display-buffer buf))
     ;; Open a directory.
     ((collab--doc-desc-dir-p doc-desc)
      (let ((root-name collab--dired-root-name))
        (select-window
         (display-buffer
          (get-buffer-create
           (if (equal (collab--doc-desc-path doc-desc) ".")
               (format "*collab dired: %s" (or root-name file-name))
             (format "*collab dired: %s/%s*" root-name file-name)))))
        (collab-dired-mode)
        (setq collab--doc-and-host (cons doc-desc host-id))
        (if (equal (collab--doc-desc-path doc-desc) ".")
            (setq collab--dired-root-name (or root-name file-name)
                  collab--dired-rel-path ".")
          (setq collab--dired-root-name root-name
                collab--dired-rel-path (collab--doc-desc-path doc-desc)))
        (collab--dired-refresh)))
     ;; Open a file.
     (t
      (collab--catch-error (format "can’t connect to %s" doc-desc)
        (let* ((resp (collab--connect-to-file-req doc-desc host-id))
               (site-id (plist-get resp :siteId))
               (content (plist-get resp :content))
               (doc-id (plist-get resp :docId))
               (file-name (plist-get resp :fileName))
               (inhibit-read-only t))
          (end-of-line)
          (unless (get-text-property (1- (point)) 'collab-status)
            (insert " " (propertize (icon-string 'collab-status-on)
                                    'collab-status t)))
          (select-window
           (display-buffer
            (generate-new-buffer
             (format "*collab: %s (%s)*" file-name doc-id))))
          (collab-monitored-mode -1)
          (erase-buffer)
          (insert content)
          (goto-char (point-min))
          (let ((buffer-file-name file-name))
            (set-auto-mode))
          (collab--enable doc-id host-id site-id)))))))

(defun collab--disconnect-from-doc ()
  "Disconnect from the file at point."
  (interactive)
  (let* ((doc-desc (get-text-property (point) 'collab-doc-desc))
         (host-id (get-text-property (point) 'collab-host-id))
         (doc-id (collab--doc-desc-id doc-desc)))
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
      (save-excursion
        (end-of-line)
        (when (looking-back " •" 2)
          (delete-char -2))))))

(defun collab--delete-doc ()
  "Delete the file at point."
  (interactive)
  (when-let ((doc-id (collab--doc-desc-id
                      (get-text-property (point) 'collab-doc-desc)))
             (host-id (get-text-property (point) 'collab-host-id)))
    (collab--catch-error (format "can’t delete Doc(%s)" doc-id)
      (collab--delete-file-req doc-id host-id))
    (collab--refresh)))

(defun collab--accept-connection ()
  "Start accepting connections."
  (interactive)
  (let ((host-id (nth 0 collab-local-server-config))
        (signaling-addr (nth 1 collab-local-server-config)))
    (collab--catch-error "can’t accept connection "
      (collab--accept-connection-req host-id signaling-addr))
    (setq-local collab--accepting-connection t)
    (collab--hub-refresh)))

(defun collab-hub ()
  "Pop up the collab hub interface."
  (interactive)
  (switch-to-buffer (collab--hub-buffer))
  (collab-hub-mode)
  (collab--hub-refresh))

(defun collab--notify-newly-shared-doc (doc-id)
  "In collab hub, insert a notice of the newly shared doc (DOC-ID)."
  (with-current-buffer (collab--hub-buffer)
    (collab--accept-connection)
    (let* ((link (format "%s/%s/%s"
                         (nth 1 collab-local-server-config)
                         (nth 0 collab-local-server-config)
                         doc-id))
           (collab--current-message
            (collab--fairy "Your file is shared, and here’s the link
Anyone can connect with just a click!
LINK: %s" (propertize link 'face 'link))))
      (collab--hub-refresh))))

(defun collab-share-buffer (file-name)
  "Share the current buffer with FILE-NAME.
When called interactively, prompt for the server."
  (interactive (list (read-string
                      "File name: "
                      (or (and buffer-file-name
                               (file-name-nondirectory buffer-file-name))
                          (buffer-name)))))
  (collab--catch-error "can’t share the current buffer"
    ;; For the moment, always share to local server.
    (let* ((server "self")
           (resp (collab--share-file-req
                  server file-name `(:reserved.source "emacs")
                  (buffer-substring-no-properties
                   (point-min) (point-max))))
           (doc-id (plist-get resp :docId))
           (site-id (plist-get resp :siteId)))
      (collab--enable doc-id server site-id)
      (display-buffer (collab--hub-buffer)
                      '(() . ((inhibit-same-window . t))))
      (collab--notify-newly-shared-doc doc-id))))

(defun collab-share-file (file file-name)
  "Share FILE with FILE-NAME."
  (interactive (let* ((path (read-file-name "Share: "))
                      (file-name (file-name-nondirectory path)))
                 (list path file-name)))
  (if (or (file-symlink-p file)
          (file-regular-p file))
      (progn
        (find-file file)
        (collab-share-buffer file-name))
    (collab-share-dir file file-name)))

(defun collab-share-dir (dir dir-name)
  "Share DIR to collab process under DIR-NAME."
  (interactive (let ((dir (read-directory-name "Share directory: ")))
                 (list (expand-file-name dir)
                       (read-string "Name: " dir))))
  (collab--catch-error "can’t share the directory"
    (let* ((resp (collab--share-dir-req "self" dir-name dir))
           (doc-id (plist-get resp :docId)))
      (collab--notify-newly-shared-doc doc-id))))

(defun collab-reconnect-buffer (server doc-id)
  "Reconnect current buffer to a remote document SERVER.

Reconnecting will override the current content of the buffer.

The document to reconnect to is determined by DOC-ID. But if
called interactively, prompt the user to select a document (by
its name rather than doc id) to connect."
  (interactive (list (completing-read
                      "Server: "
                      (mapcar #'car (collab--server-alist)))
                     'interactive))
  (collab--catch-error (format "can’t reconnect to Doc(%s)" doc-id)
    (let* ((info (alist-get server (collab--server-alist)
                            nil nil #'equal))
           (file-list (collab--list-files-req
                       nil server (nth 0 info) (nth 1 info)))
           doc-desc
           file-name)

      ;; Get file name by doc id, or prompt for file name and get doc
      ;; id by file name.
      (if (eq doc-id 'interactive)
          (progn
            (setq file-name (completing-read
                             "Merge with: "
                             (mapcar (lambda (elm)
                                       (plist-get elm :fileName))
                                     file-list)
                             nil t))
            (setq doc-desc (cl-loop for elm in file-list
                                    if (equal (plist-get elm :fileName)
                                              file-name)
                                    return (plist-get elm :docDesc))))

        (setq file-name (cl-loop for elm in file-list
                                 if (equal (collab--doc-desc-id
                                            (plist-get elm :docDesc))
                                           doc-id)
                                 return (plist-get elm :fileName)))
        (setq doc-desc `(:Doc ,doc-id)))

      ;; Replace buffer content with document content.
      (let* ((resp (collab--connect-to-file-req doc-desc server))
             (content (plist-get resp :content))
             (site-id (plist-get resp :siteId))
             (inhibit-read-only t))

        (when (buffer-modified-p)
          (when (y-or-n-p "Save buffer before merging?")
            (save-buffer)))

        ;; Same as in ‘collab--open-doc’.
        (collab-monitored-mode -1)
        (erase-buffer)
        (insert content)
        (goto-char (point-min))
        (let ((buffer-file-name file-name))
          (set-auto-mode))
        (collab--enable doc-id server site-id)))))

(defun collab-disconnect-buffer ()
  "Disconnect current buffer, returning it to a regular buffer."
  (interactive)
  (collab--check-precondition)
  (let ((doc-id (car collab--doc-and-host))
        (host-id (cdr collab--doc-and-host)))
    (collab--catch-error
        (format "can’t disconnect from Doc(%s)" doc-id)
      (collab--disconnect-from-file-req doc-id host-id)))
  (collab--disable)
  (message "Disconnected"))

(defun collab-share-link ()
  "Copy the share link of current doc to clipboard."
  (interactive)
  (if-let* ((doc-id (car collab--doc-and-host))
            (link (format "%s/%s/%s"
                          (nth 1 collab-local-server-config)
                          (nth 0 collab-local-server-config)
                          doc-id)))
      (progn
        (kill-new link)
        (message "Copied to clipboard: %s" link))
    (message "Couldn’t find the doc id of this buffer")))

(defun collab-connect (share-link)
  "Connect to the file at SHARE-LINK.
SHARE-LINK should be in the form of signaling-server/host-id/doc-id."
  (interactive (list (read-string "Share link: ")))
  (let* ((share-link (string-trim-right share-link "/"))
         doc-id host-id signaling-addr)
    (setq doc-id (string-to-number (file-name-nondirectory share-link)))
    (setq share-link (string-trim-right
                      (file-name-directory share-link) "/"))
    (setq host-id (file-name-nondirectory share-link))
    (setq share-link (string-trim-right
                      (file-name-directory share-link) "/"))
    (setq signaling-addr share-link)
    (unless (alist-get host-id collab-server-alist nil nil #'equal)
      (push (cons host-id (list signaling-addr ""))
            collab-server-alist))

    ;; TODO Only connect to server if we haven’t yet.
    (collab--catch-error (format "can’t connect to Server %s" host-id)
      (collab--list-files-req nil host-id signaling-addr ""))

    (collab--open-doc doc-id host-id)
    (save-excursion
      (with-current-buffer (collab--hub-buffer)
        (collab--hub-refresh)))))

(defun collab--print-history (&optional debug)
  "Print debugging history for the current buffer.
If called with an interactive argument (DEBUG), print more
detailed history."
  (interactive "p")
  (collab--check-precondition)
  (let ((doc-id (car collab--doc-and-host))
        (host-id (cdr collab--doc-and-host))
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

(defun collab ()
  "The main entry point of ‘collab-mode’."
  (interactive)
  (unless (and collab-display-name collab-local-server-config)
    (collab-initial-setup))
  (let ((resp (read-multiple-choice
               (collab--fairy "Heya! It’s nice to see you! Tell me, what do you want to do? ")
               '((?s "share this buffer" "Share this buffer to a server")
                 (?d "share a directory" "Share a directory")
                 (?r "reconnect to doc" "Reconnect to a document")
                 (?q "quit (disconnect)" "Disconnect and stop collaboration")
                 (?h "hub" "Open collab hub")))))
    (pcase (car resp)
      (?s (when (or (null collab-monitored-mode)
                    (y-or-n-p (collab--fairy "Buffer in collab-mode, already quite set. Share it as new doc, confirm without regret? ")))
            (call-interactively #'collab-share-buffer)))
      (?d (call-interactively #'collab-share-dir))
      (?r (call-interactively #'collab-reconnect-buffer))
      (?q (collab-disconnect-buffer))
      (?h (collab-hub)))))

;;; Setup

(defun collab-initial-setup ()
  "Initial setup wizard. Set display name, download binary, etc."
  (let ((display-name (read-string (collab--fairy "Heya! I’m dolly dolly dolly, the collab-mode fairy. Sweet human, tell me, what name do you carry? -- ") user-full-name))
        (local-host-id (collab--uuid))
        (default-signal-server "wss://signal.collab-mode.org"))
    (customize-set-variable 'collab-display-name display-name)
    (customize-set-variable 'collab-local-server-config
                            (list local-host-id default-signal-server))
    (customize-save-customized)

    (when (and (not (executable-find "collab-mode"))
               (y-or-n-p (collab--fairy "No binary here, a tiny regret. Shall we fetch from the internet? ")))
      (let ((choice (car (read-multiple-choice
                          (collab--fairy "Which one suits you the best? ")
                          '((?l "linux_x64" "Linux x86_64")
                            (?m "mac_x64" "Mac x86_64")
                            (?a "mac_arm" "Mac ARM"))))))
        (ignore choice)
        (message "TODO download binary")))))


(provide 'collab-mode)

;;; collab.el ends here
