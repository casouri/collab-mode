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

(defvar collab-rpc-timeout 1
  "Timeout in seconds to wait for connection to collab process.")

(defvar collab-command "~/p/collab/target/debug/collab"
  "Path to the collab server executable.")

(defvar collab-connection-type '(socket 7701)
  "How to connect to the collab process.
The value can be ‘pipe’, meaning use stdio for connection,
or (socket PORT), meaning using a socket connection, and PORT
should be the port number.")

(defvar collab-local-server-config '("#1" "ws://127.0.0.1:8822")
  "A list storing configuration for the local server.
The list should be (SERVER-ID SIGNALING-SERVER-ADDR).")

(defvar collab-server-alist '(("#2" . ("ws://127.0.0.1:8822" "")))
  "An alist of server configurations.
Each key is SERVER-ID, each value is (SIGNALING-SERVER-ADDR
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
  (list (string-to-number (substring color 1 3) 16)
        (string-to-number (substring color 3 5) 16)
        (string-to-number (substring color 5 7) 16)))

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
    (-32099 . ServerErrorStart)
    (-32000 . ServerErrorEnd)

    (-31000 . ConnectionBroke)
    (-30001 . OTEngineError)
    (-30002 . PermissionDenied)
    (-30003 . DocNotFound)
    (-30004 . DocAlreadyExists))
  "An alist of JSONRPC error codes.")

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
       (error
        (when collab-monitored-mode
          (collab--disable))
        (display-warning 'collab
                         (format "collab %s: %s" ,msg ,err))))))

(defvar collab--jsonrpc-connection)
(defvar collab--doc-server)
(defun collab--check-precondition ()
  "Check ‘collab--jsonrpc-connection’ and ‘collab--doc-server’.

If they are invalid, turn off ‘collab-monitored-mode’ and raise
an error. This function should be called before any interactive
command that acts on a collab-monitored buffer."
  (unless collab--jsonrpc-connection
    (collab-monitored-mode -1)
    (display-warning 'collab "Connection to collab server broke"))
  (unless collab--doc-server
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
  "Hash table mapping (doc-id . server-id) to sync cursor timers.")

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
                                           nil nil nil)))
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
      (let* ((doc-id (car collab--doc-server))
             (server-id (cdr collab--doc-server))
             (site-id collab--my-site-id))
        (collab--send-info-req
         doc-id server-id
         (if (region-active-p)
             `( :type "_pos" :point ,(point)
                :mark ,(mark) :siteId ,site-id)
           `(:type "_pos" :point ,(point) :siteId ,site-id)))))

    (remhash collab--doc-server
             collab--sync-cursor-timer-table)))

(defun collab--send-cursor-pos ()
  "Move user (SITE-ID)’s cursor overlay to POS."
  (collab--check-precondition)
  (when (null (gethash collab--doc-server
                       collab--sync-cursor-timer-table))
    ;; Run with an idle timer to we don’t interrupt scrolling, etc.
    (let ((timer (run-with-idle-timer
                  0.5 nil #'collab--send-cursor-pos-1
                  (current-buffer))))
      (puthash collab--doc-server timer
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

(defvar-local collab--doc-server nil
  "(DOC-ID . SERVER-ID) for the current buffer.")

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

               (let ((old (substring content (- beg rbeg) len))
                     (current (buffer-substring-no-properties beg end)))
                 (if (eq beg end)
                     ;; b) Delete.
                     (push (list 'del beg old group-seq) ops)
                   (if (equal old current)
                       ;; c) Text prop change.
                       nil
                     ;; d) Replace.
                     (push (list 'ins beg current group-seq) ops)
                     (push (list 'del beg old group-seq) ops))))

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

        (setq-local buffer-undo-list t)

        (setq-local collab--pending-ops nil)
        (setq-local collab--ops-current-command nil)
        (setq-local collab--group-seq 1))

    ;; Disable.
    (remhash collab--doc-server
             collab--dispatcher-timer-table)

    (remove-hook 'before-change-functions #'collab--before-change t)
    (remove-hook 'after-change-functions #'collab--after-change t)
    (remove-hook 'pre-command-hook #'collab--pre-command t)
    (remove-hook 'post-command-hook #'collab--post-command t)
    (remove-hook 'post-command-hook #'collab--post-command-hasty t)

    (remhash collab--doc-server
             collab--buffer-table)

    (dolist (ov collab--cursor-ov-alist)
      (delete-overlay (cdr ov)))
    (setq-local collab--cursor-ov-alist nil)

    (setq-local collab--doc-server nil)
    (setq-local collab--my-site-id nil)
    (setq-local buffer-undo-list nil)))

(defun collab--enable (doc-id server-id my-site-id)
  "Enable ‘collab-monitored-mode’ in the current buffer.
DOC-ID and SERVER-ID are associated with the current buffer.
MY-SITE-ID is the site if of this editor."
  (setq collab--doc-server (cons doc-id server-id))
  (setq collab--my-site-id my-site-id)
  (puthash collab--doc-server
           (current-buffer)
           collab--buffer-table)
  (collab-monitored-mode)
  (let ((pulse-delay 0.1))
    (pulse-momentary-highlight-region
     (point-min) (point-max) 'diff-refine-added)))

(defun collab--disable ()
  "Disable ‘collab’ for the current buffer and flash red."
  (let ((pulse-delay 0.1))
    (pulse-momentary-highlight-region
     (point-min) (point-max) 'diff-refine-removed))
  (collab-monitored-mode -1))

(defvar collab--stashed-state-plist nil
  "Used for stashing local state when major mode changes.")

(defun collab--rescue-state ()
  "Stash local variable elsewhere before major mode changes."
  (setq collab--stashed-state-plist
        (plist-put collab--stashed-state-plist
                   (current-buffer)
                   `( :doc-server ,collab--doc-server
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

;;;; Connection

(defvar collab--jsonrpc-connection nil
  "The JSONRPC connection to the collab server.")

(defun collab--connect ()
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
  "Hash table mapping (doc-id . server-id) to dispatcher timers.")


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

(defvar collab--hub-buffer)
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
           (server (plist-get params :serverId))
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
            (server-id (plist-get params :serverId))
            (value (plist-get params :value)))
       (pcase (plist-get value :type)
         ("_pos" (let* (
                        (pos (plist-get value :point))
                        (mark (plist-get value :mark))
                        (site-id (plist-get value :siteId))
                        (buf (gethash (cons doc-id server-id)
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
     (run-with-timer
      0 nil
      (lambda ()
        (with-current-buffer collab--hub-buffer
          (setq collab--accepting-connection nil)
          (collab--refresh)
          (message "Collab process stopped accepting connections")))))
    ('AcceptConnectionErr
     (run-with-timer
      0 nil
      (lambda ()
        (with-current-buffer collab--hub-buffer
          (setq collab--accepting-connection nil)
          (collab--refresh)
          (display-warning
           'collab
           (format "Error accepting remote peer connections: %s" params))))))))

;;;; Requests

(defun collab--list-files-req (server signaling-server credential)
  "Return a list of files on SERVER (address) with CREDENTIAL.
SIGNALING-SERVER is the address of the signaling server. Each
file is of the form (:docId DOC-ID :fileName FILE-NAME)."
  (let* ((conn (collab--connect))
         (resp (jsonrpc-request
                conn 'ListFiles
                `( :serverId ,server
                   :signalingAddr ,signaling-server
                   :credential ,credential)
                :timeout collab-rpc-timeout)))
    (seq-map #'identity (plist-get resp :files))))

(defun collab--share-file-req (server filename content)
  "Share the file with SERVER (address).
FILENAME is filename, CONTENT is just the content of the file in
a string. Return (:docId DOC-ID :siteId SITE-ID). If FORCE is
non-nil, override existing files."
  (let* ((conn (collab--connect))
         (resp (jsonrpc-request
                conn 'ShareFile
                `( :serverId ,server
                   :fileName ,filename
                   :content ,content)
                :timeout collab-rpc-timeout)))
    resp))

(defun collab--connect-to-file-req (doc server)
  "Connect to DOC (doc id) on SERVER (server address).
Return (:siteId SITE-ID :content CONTENT)."
  (let* ((conn (collab--connect))
         (resp (jsonrpc-request
                conn 'ConnectToFile
                `( :serverId ,server
                   :docId ,doc)
                :timeout collab-rpc-timeout)))
    resp))

(defun collab--disconnect-from-file-req (doc server)
  "Disconnect from DOC on SERVER."
  (let ((conn (collab--connect)))
    (jsonrpc-request conn 'DisconnectFromFile
                     `( :serverId ,server
                        :docId ,doc)
                     :timeout collab-rpc-timeout)))

(defun collab--delete-file-req (doc server)
  "Delete DOC on SERVER."
  (let ((conn (collab--connect)))
    (jsonrpc-request conn 'DeleteFile
                     `( :serverId ,server
                        :docId ,doc)
                     :timeout collab-rpc-timeout)))

(defun collab--accept-connection-req (server-id signaling-addr)
  "Accept connections as SERVER-ID on SIGNALING-ADDR."
  (let ((conn (collab--connect)))
    (jsonrpc-request conn 'AcceptConnection
                     `( :serverId ,server-id
                        :signalingAddr ,signaling-addr)
                     :timeout collab-rpc-timeout)))

(defun collab--print-history-req (doc server debug)
  "Print debugging history for (DOC SERVER).
If DEBUG non-nil, print debug version."
  (let ((conn (collab--connect)))
    (jsonrpc-request conn 'PrintHistory
                     `( :docId ,doc
                        :serverId ,server
                        :debug ,(if debug t :json-false))
                     :timeout collab-rpc-timeout)))

(defun collab--send-info-req (doc server info)
  "Send INFO to DOC on SERVER.
INFO should be a plist JSON object. This request is async."
  (let ((conn (collab--connect)))
    (jsonrpc-notify conn 'SendInfo
                    `( :serverId ,server
                       :docId ,doc
                       :info ,info))))

(defsubst collab--encode-op (op)
  "Encode Elisp OP into JSONRPC format."
  (pcase op
    (`(ins ,pos ,str ,group-seq)
     `( :op (:Ins [,(1- pos) ,str])
        :groupSeq ,group-seq
        :kind "Original"))

    (`(del ,pos ,str ,group-seq)
     `( :op (:Del [[,(1- pos) ,str]])
        :groupSeq ,group-seq
        :kind "Original"))))

(defsubst collab--apply-jrpc-op (op &optional move-point)
  "Decode JSPNRPC OP into Elisp format.
If MOVE-POINT non-nil, move point as the edit would."
  (let ((inhibit-modification-hooks t)
        (collab--inhibit-hooks t)
        (start (point-marker)))
    (pcase (plist-get op :op)
      (`(:Ins [,pos ,str])
       (goto-char (1+ pos))
       (insert str))
      (`(:Del ,edits)
       (when (> (length edits) 0)
         (cl-loop for idx from (1- (length edits)) to 0
                  for edit = (aref edits idx)
                  do (delete-region (1+ (aref edit 0))
                                    (1+ (+ (aref edit 0)
                                           (length (aref edit 1)))))))))
    (unless move-point
      (goto-char start))))

(defun collab--send-ops (ops &optional encoded)
  "Send OPS to the collab server and apply returned remote ops.

If ENCODED is non-nil, OPS should be already in sexp JSON
format (a list of EditorOp), and this function will skip encoding
it.

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
             `( :docId ,(car collab--doc-server)
                :serverId ,(cdr collab--doc-server)
                :ops ,(if (not encoded)
                          (if ops
                              (vconcat (mapcar #'collab--encode-op
                                               ops))
                            [])
                        ops))
             :timeout collab-rpc-timeout))
      ;; Only ‘seq-map’ can map over vector.
      (let ((remote-ops (plist-get resp :ops))
            (last-seq (plist-get resp :lastSeq))
            last-op)
        (seq-map (lambda (op)
                   (collab--apply-jrpc-op op)
                   (setq last-op op))
                 remote-ops)
        (pcase last-op
          (`(:op (:Ins [,pos ,str]) :siteId ,site-id)
           (collab--move-cursor site-id (+ 1 pos (length str))))
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
                             `( :serverId ,(cdr collab--doc-server)
                                :docId ,(car collab--doc-server)
                                :kind ,(if redo "Redo" "Undo"))
                             :timeout collab-rpc-timeout))
      (if-let ((json-ops (plist-get resp :ops)))
          (if (eq (length json-ops) 0)
              (message "No more operations to %s" (if redo "redo" "undo"))
            ;; Only ‘seq-map’ can map over vector. TODO: if the op is
            ;; identify, undo one more step.
            (seq-map (lambda (json-op)
                       (collab--apply-jrpc-op json-op t))
                     json-ops)
            (collab--send-ops
             (vconcat (seq-map (lambda (json-op)
                                 `( :op ,json-op
                                    :groupSeq ,collab--group-seq
                                    :kind ,(if redo "Redo" "Undo")))
                               json-ops))
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

(defvar collab--hub-buffer "*collab*"
  "Buffer name for the hub buffer.")

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

(defun collab--insert-file (doc-id file-name server)
  "Insert file that has DOC-ID and FILE-NAME.
SERVER is its server id."
  (let ((beg (point)))
    (insert file-name)
    (add-text-properties
     beg (point) `( collab-doc-id ,doc-id
                    collab-file-name ,file-name))
    (when (collab--doc-connected doc-id server)
      (insert " " (propertize (icon-string 'collab-status-on)
                              'collab-status t)))))

(defun collab--insert-server (server signaling-server credential)
  "Insert SERVER (server id) on SIGNALING-SERVER and its files.
Server has CREDENTIAL. SIGNALING-SERVER is the address of the
signaling server."
  (let ((beg (point)))
    ;; 1. Insert server line.
    (insert (propertize server 'face 'bold
                        'collab-server-line t))
    (unless (equal server "self")
      (insert (propertize " UP" 'face 'success)))
    (insert (propertize "\n" 'line-spacing 0.4))
    (let ((files (collab--list-files-req
                  server signaling-server credential)))
      ;; 2. Insert files.
      (if files
          (dolist (file files)
            (let ((doc-id (plist-get file :docId))
                  (file-name (plist-get file :fileName)))
              (collab--insert-file doc-id file-name server)
              (insert "\n")))
        (insert (propertize "(empty)\n" 'face 'shadow)))
      ;; 4. Mark server section.
      (add-text-properties beg (point)
                           `(collab-server-id ,server)))))

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
                         `(collab-server-id ,server))))

(defun collab--prop-section (prop)
  "Return the (BEG . END) of the range that has PROP."
  (save-excursion
    (let ((forward-prop (text-property-search-forward prop))
          (backward-prop (text-property-search-backward prop)))
      (cons (prop-match-beginning backward-prop)
            (prop-match-end forward-prop)))))

(defvar collab--hub-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "RET") #'collab--open-file)
    (define-key map (kbd "x") #'collab--delete-file)
    (define-key map (kbd "k") #'collab--disconnect-from-file)
    (define-key map (kbd "g") #'collab--refresh)
    (define-key map (kbd "A") #'collab--accept-connection)

    (define-key map (kbd "n") #'next-line)
    (define-key map (kbd "p") #'previous-line)
    map)
  "Keymap for ‘collab--hub-mode’.")

(define-derived-mode collab--hub-mode special-mode "Collab"
  "Collaboration mode."
  (setq-local line-spacing 0.2
              eldoc-idle-delay 0.8)
  (add-hook 'eldoc-documentation-functions #'collab--eldoc 0 t)
  (add-hook 'post-command-hook #'collab--dynamic-highlight)
  (eldoc-mode))

(defun collab--render ()
  "Render collab mode buffer."
  (let ((connection-up nil))
    ;; Insert headers.
    (insert "Connection: ")
    (condition-case err
        (progn
          (collab--connect)
          (insert (propertize "UP" 'face 'success) "\n")
          (setq connection-up t))
      (error
       (insert (propertize "DOWN" 'face 'error) "\n")
       (setq collab--most-recent-error
             (format "Cannot connect to collab process: %s"
                     (error-message-string err)))))
    (insert (format "Connection type: %s\n" collab-connection-type))
    (insert "Accepting remote peer connections: "
            (if collab--accepting-connection
                (propertize "YES" 'face 'success)
              (propertize "NO" 'face 'shadow))
            "\n")
    (insert "Errors: ")
    (insert "0" "\n")
    (insert "\n")

    ;; Insert each server and files.
    (dolist (entry (collab--server-alist))
      (let ((beg (point)))
        (condition-case err
            (if connection-up
                (collab--insert-server
                 (car entry) (nth 0 (cdr entry)) (nth 1 (cdr entry)))
              (collab--insert-disconnected-server (car entry)))
          (jsonrpc-error
           (delete-region beg (point))
           (collab--insert-disconnected-server (car entry) t)
           (setq collab--most-recent-error
                 (format "Error connecting to remote peer: %s" err)))))
      ;; Insert an empty line after a server section.
      (insert "\n"))

    (insert "PRESS + TO CONNECT to REMOTE SERVER\n")
    (insert "PRESS A TO ACCEPT REMOTE CONNECTIONS (for 180s)\n")
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
     ((get-text-property (point) 'collab-doc-id)
      (let ((range (collab--prop-section 'collab-doc-id)))
        (move-overlay ov (car range) (cdr range)))
      (setq-local cursor-type nil))
     ((get-text-property (point) 'collab-server-line)
      (let ((range (collab--prop-section 'collab-server-id)))
        (move-overlay ov (car range) (cdr range)))
      (setq-local cursor-type nil))
     (t
      (when ov
        (move-overlay ov 1 1))
      (kill-local-variable 'cursor-type)))))

;;;; Eldoc

(defun collab--eldoc (callback)
  "Displays help information at point.
Used for ‘eldoc-documentation-functions’, calls CALLBACK
immediately."
  (let ((doc-id (get-text-property (point) 'collab-doc-id))
        (server-id (get-text-property (point) 'collab-server-id)))
    (cond
     (doc-id
      (funcall callback
               "RET → Open File  x → Delete File  k → Disconnect"))
     (server-id
      (funcall callback "s → Share File"))
     (t
      (funcall callback "g → Refresh")))))

;;;; Interactive commands

(defun collab-shutdown ()
  "Shutdown the connection to the collab process."
  (interactive)
  (when collab--jsonrpc-connection
    (jsonrpc-shutdown collab--jsonrpc-connection)
    (setq collab--jsonrpc-connection nil)))

(defun collab--refresh ()
  "Refresh collab buffer."
  (interactive)
  (let ((inhibit-read-only t)
        (line (line-number-at-pos)))
    (erase-buffer)
    (collab--render)
    (goto-char (point-min))
    (forward-line (1- line))))

(defun collab--open-file ()
  "Open the file at point."
  (interactive)
  (let* ((doc-id (get-text-property (point) 'collab-doc-id))
         (file-name (get-text-property (point) 'collab-file-name))
         (server-id (get-text-property (point) 'collab-server-id))
         (buf (gethash (cons doc-id server-id)
                       collab--buffer-table)))
    (if (and buf (buffer-live-p buf))
        (pop-to-buffer buf)
      (collab--catch-error (format "can’t connect to Doc(%s)" doc-id)
        (let* ((resp (collab--connect-to-file-req doc-id server-id))
               (site-id (plist-get resp :siteId))
               (content (plist-get resp :content))
               (inhibit-read-only t))
          (end-of-line)
          (unless (get-text-property (1- (point)) 'collab-status)
            (insert " " (propertize (icon-string 'collab-status-on)
                                    'collab-status t)))
          (pop-to-buffer
           (generate-new-buffer (concat "*collab: " file-name "*")))
          (collab-monitored-mode -1)
          (erase-buffer)
          (insert content)
          (goto-char (point-min))
          (let ((buffer-file-name file-name))
            (set-auto-mode))
          (collab--enable doc-id server-id site-id))))))

(defun collab--disconnect-from-file ()
  "Disconnect from the file at point."
  (interactive)
  (let ((doc-id (get-text-property (point) 'collab-doc-id))
        (server-id (get-text-property (point) 'collab-server-id)))
    (collab--catch-error
        (format "can’t disconnect from Doc(%s)" doc-id)
      (collab--disconnect-from-file-req doc-id server-id))
    (let ((buf (gethash (cons doc-id server-id)
                        collab--buffer-table))
          (inhibit-read-only t))
      (when buf
        (with-current-buffer buf
          (collab--disable)))
      (save-excursion
        (end-of-line)
        (when (looking-back " •" 2)
          (delete-char -2))))))

(defun collab--delete-file ()
  "Delete the file at point."
  (interactive)
  (let ((doc-id (get-text-property (point) 'collab-doc-id))
        (server-id (get-text-property (point) 'collab-server-id)))
    (collab--catch-error (format "can’t delete Doc(%s)" doc-id)
      (collab--delete-file-req doc-id server-id))
    (collab--refresh)))

(defun collab--accept-connection ()
  "Start accepting connections."
  (interactive)
  (let ((server-id (nth 0 collab-local-server-config))
        (signaling-addr (nth 1 collab-local-server-config)))
    (collab--catch-error "can’t accept connection "
      (collab--accept-connection-req server-id signaling-addr))
    (setq-local collab--accepting-connection t)
    (collab--refresh)))

(defun collab ()
  "Pop up the collab hub interface."
  (interactive)
  (switch-to-buffer (get-buffer-create collab--hub-buffer))
  (collab--hub-mode)
  (collab--refresh))

(defun collab-share-buffer (server file-name)
  "Share the current buffer to SERVER under FILE-NAME.
When called interactively, prompt for the server."
  (interactive (list (completing-read
                      "Server: "
                      (mapcar #'car (collab--server-alist)))
                     (read-string
                      "File name: "
                      (or (and buffer-file-name
                               (file-name-nondirectory buffer-file-name))
                          (buffer-name)))))
  (collab--catch-error "can’t share the current buffer"
    (let* ((resp (collab--share-file-req
                  server file-name
                  (buffer-substring-no-properties (point-min) (point-max))))
           (doc-id (plist-get resp :docId))
           (site-id (plist-get resp :siteId)))
      (collab--enable doc-id server site-id))))

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
                       server (nth 0 info) (nth 1 info)))
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
            (setq doc-id (cl-loop for elm in file-list
                                  if (equal (plist-get elm :fileName)
                                            file-name)
                                  return (plist-get elm :docId))))

        (setq file-name (cl-loop for elm in file-list
                                 if (equal (plist-get elm :docId)
                                           doc-id)
                                 return (plist-get elm :fileName))))

      ;; Replace buffer content with document content.
      (let* ((resp (collab--connect-to-file-req doc-id server))
             (content (plist-get resp :content))
             (site-id (plist-get resp :siteId))
             (inhibit-read-only t))

        (when (buffer-modified-p)
          (when (y-or-n-p "Save buffer before merging?")
            (save-buffer)))

        ;; Same as in ‘collab--open-file’.
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
  (let ((doc-id (car collab--doc-server))
        (server-id (cdr collab--doc-server)))
    (collab--catch-error
        (format "can’t disconnect from Doc(%s)" doc-id)
      (collab--disconnect-from-file-req doc-id server-id)))
  (collab--disable)
  (message "Disconnected"))

(defun collab--print-history (&optional debug)
  "Print debugging history for the current buffer.
If called with an interactive argument (DEBUG), print more
detailed history."
  (interactive "p")
  (collab--check-precondition)
  (let ((doc-id (car collab--doc-server))
        (server-id (cdr collab--doc-server))
        (debug (eq debug 4)))
    (collab--catch-error
        (format "can’t print history of Doc(%s)" doc-id)
      (collab--send-ops-now)
      (let ((text (collab--print-history-req doc-id server-id debug))
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

(defun collab-mode ()
  "The main entry point of collab-mode, select an operation to perform."
  (interactive)
  (let ((resp (read-multiple-choice
               "Heya! What do you want to do? "
               '((?s "share this buffer" "Share this buffer to a server")
                 (?r "reconnect to doc" "Reconnect to a document")
                 (?d "disconnect" "Disconnect and stop collaboration")
                 (?h "hub" "Open collab hub")))))
    (pcase (car resp)
      (?s (call-interactively #'collab-share-buffer))
      (?r (call-interactively #'collab-reconnect-buffer))
      (?d (collab-disconnect-buffer))
      (?h (collab)))))

;;; Debug

(defun collab--setup-2 ()
  "Setup test session #2."
  (interactive)
  (setq collab-server-alist '(("#1" . ("ws://127.0.0.1:8822" "blah"))))
  (setq collab-local-server-config '("#2" "ws://127.0.0.1:8822"))
  (setq collab-connection-type '(socket 7702)))

(provide 'collab-mode)

;;; collab.el ends here
