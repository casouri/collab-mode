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

(defgroup collab-mode
  ()
  "Collaboration mode."
  :group 'editting)

(defvar collab-mode-rpc-timeout 1
  "Timeout in seconds to wait for connection to collab process.")

(defvar collab-mode-command "~/p/collab-mode/target/debug/collab-mode"
  "Path to the collab server executable.")

(defvar collab-mode-connection-type '(socket 7701)
  "How to connect to the collab process.
The value can be ‘pipe’, meaning use stdio for connection,
or (socket PORT), meaning using a socket connection, and PORT
should be the port number.")

(defvar collab-mode-local-server-config '("#1" "ws://127.0.0.1:8822")
  "A list storing configuration for the local server.
The list should be (SERVER-ID SIGNALING-SERVER-ADDR).")

(defvar collab-mode-server-alist '(("#2" . ("ws://127.0.0.1:8822" "#2")))
  "An alist of server configurations.
Each key is SERVER-ID, each value is (SIGNALING-SERVER-ADDR
CREDENTIAL).")

(defvar collab-mode-hasty-p t
  "If non-nil, buffer changes are sent to collab process immediately.")

;; Generated with seaborn.color_palette("colorblind").as_hex()
(defvar collab-mode-cursor-colors
  '( "#0173b2" "#de8f05" "#029e73" "#d55e00" "#cc78bc" "#ca9161"
     "#fbafe4" "#949494" "#ece133" "#56b4e9")
  "Cursor color ring.")

;;; Icons

(defvar collab-mode--load-directory
  (file-name-directory (or load-file-name buffer-file-name))
  "Directory in which collab-mode.el resides.")

(define-icon collab-status-on nil
  `((image ,(concat collab-mode--load-directory "/dot_medium_16.svg")
           :face success
           :height (0.9 . em)
           :ascent 83)
    (symbol "•")
    (text "*"))
  "Icon for collab-mode on status indicator."
  :version "30.1"
  :help-echo "ON")

(define-icon collab-status-off nil
  `((image ,(concat collab-mode--load-directory "/dot_medium_16.svg")
           :face error
           :height (0.9 . em)
           :ascent 83)
    (symbol "•")
    (text "*"))
  "Icon for collab-mode off status indicator."
  :version "30.1"
  :help-echo "OFF")

;;; Error handling

(defvar collab-mode--error-code-alist
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

(defmacro collab-mode--catch-error (msg &rest body)
  "Execute BODY, catch jsonrpc errors.
If there’s an error, print “Collab-mode MSG: ERROR-DESCRIPTION”,
and append the error to collab error buffer (*collab errors*).
MSG should be something like “can’t do xxx”."
  (declare (indent 1) (debug (sexp &rest form)))
  (let ((err (gensym)))
    `(condition-case ,err
         (progn
           ,@body)
       (error
        (when collab-monitored-mode
          (collab-mode--disable))
        (display-warning 'collab-mode
                         (format "Collab-mode %s: %s" ,msg ,err))))))

(defvar collab-mode--jsonrpc-connection)
(defvar collab-mode--doc-server)
(defun collab-mode--check-precondition ()
  "Check ‘collab-mode--jsonrpc-connection’ and ‘collab-mode--doc-server’.

If they are invalid, turn off ‘collab-monitored-mode’ and raise
an error. This function should be called before any interactive
command that acts on a collab-monitored buffer."
  (unless collab-mode--jsonrpc-connection
    (collab-monitored-mode -1)
    (display-warning 'collab-mode "Connection to collab server broke"))
  (unless collab-mode--doc-server
    (collab-monitored-mode -1)
    (display-warning
     'collab-mode "Current buffer doesn’t have a doc id or server id")))


;;; Cursor

(defvar-local collab-mode--cursor-ov-alist nil
  "An alist mapping user’s site id to cursor overlays.")

(defvar-local collab-mode--my-site-id nil
  "My site-id.")

(defun collab-mode--move-cursor (site-id pos)
  "Move user (SITE-ID)’s cursor overlay to POS."
  (when (and (not (eq site-id collab-mode--my-site-id))
             (<= (point-min) pos (point-max)))
    (let* ((idx (mod site-id
                     (length collab-mode-cursor-colors)))
           (face `(:background
                   ,(nth idx collab-mode-cursor-colors)))
           (ov (or (alist-get site-id collab-mode--cursor-ov-alist)
                   (let ((ov (make-overlay (min (1+ pos) (point-max))
                                           (min (+ pos 2) (point-max))
                                           nil nil nil)))
                     (overlay-put ov 'face face)
                     (push (cons site-id ov) collab-mode--cursor-ov-alist)
                     ov))))

      (if (eq pos (point-max))
          (progn
            (overlay-put ov 'after-string (propertize " " 'face face))
            (move-overlay ov pos (min (1+ pos) (point-max))))
        (overlay-put ov 'after-string nil)
        (move-overlay ov pos (1+ pos))))))


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

(defvar-local collab-mode--doc-server nil
  "(DOC-ID . SERVER-ID) for the current buffer.")

(defvar-local collab-mode--accepting-connection nil
  "If non-nil, our collab process is accepting remote connections.")

(defvar-local collab-mode--most-recent-error nil
  "Most recent error message.")

(defvar collab-mode--buffer-table (make-hash-table :test #'equal)
  "A has table that maps (DOC . SERVER) to their corresponding buffer.")

(defvar-local collab-mode--inhibit-hooks nil
  "When non-nil, post command hooks don’t run.")

(defvar-local collab-mode--changing-region nil
  "Records information of the region to be changed.
The shape is (beg end content), where CONTENT is the buffer
content between BEG and END.")

(defvar-local collab-mode--pending-ops nil
  "Ops waiting to be sent out. In reverse order.")

(defvar-local collab-mode--ops-current-command nil
  "Ops created in the current command. In reverse order.")

(defvar-local collab-mode--group-seq nil
  "Group sequence number.
Group sequence is used to mark undo groups. consecutive ops with
the same group sequence are undone together.")

(defvar collab-mode--edit-tracking-timer nil
  "An idle timer that sends pending ops to collab process.")

(defvar collab-mode--idle-timer-registered-buffer nil
  "If a buffer has pending ops, it adds itself to this list.
So that the idle timer can send its pending ops.")

(defun collab-mode--server-alist ()
  "Return ‘collab-mode--server-alist’ plus “self”."
  (cons '("self" . ("" ""))
        collab-mode-server-alist))

(defun collab-mode--before-change (beg end)
  "Records the region (BEG . END) to be changed."
  (when (not (eq beg end))
    (setq collab-mode--changing-region
          (list beg end (buffer-substring-no-properties beg end)))))

(defun collab-mode--after-change (beg end len)
  "Record the changed region (BEG END LEN)."
  (let ((error-fn (lambda ()
                    (display-warning
                     'collab-mode
                     "Buffer change not correctly recorded (exceeds recorded region: %s)"
                     collab-mode--changing-region)
                    (collab-monitored-mode -1)
                    (throw 'term nil)))
        (group-seq collab-mode--group-seq)
        ops)
    (catch 'term
      (if (eq len 0)
          ;; a) Insert.
          (push (list 'ins beg (buffer-substring-no-properties beg end)
                      group-seq)
                ops)
        ;; Not insert.
        (pcase collab-mode--changing-region
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

      (setq collab-mode--changing-region nil)
      (setq collab-mode--ops-current-command
            (nconc ops collab-mode--ops-current-command)))))

(defun collab-mode--pre-command ()
  "Increment ‘collab-mode--group-seq’."
  (when (numberp collab-mode--group-seq)
    (cl-incf collab-mode--group-seq)))

(defun collab-mode--post-command-hasty ()
  "Like ‘collab-mode--post-command’ but just send ops out."
  (when (and (not collab-mode--inhibit-hooks)
             collab-mode--ops-current-command)
    (collab-mode--send-ops (nreverse collab-mode--ops-current-command))
    (setq collab-mode--ops-current-command nil)))

(defun collab-mode--post-command ()
  "Convert ops in the buffer and send them to the collab process.
Then apply the returned remote ops."
  (when (and (not collab-mode--inhibit-hooks)
             collab-mode--ops-current-command)
    (if-let ((amalgamated-op
              (and (eq 1 (length collab-mode--ops-current-command))
                   (let ((this-op (car collab-mode--ops-current-command))
                         (last-op (car collab-mode--pending-ops)))
                     (and last-op
                          (collab-mode--maybe-amalgamate
                           last-op this-op))))))
        ;; Amalgamated, don’t send ops yet.
        (progn
          (setcar collab-mode--pending-ops amalgamated-op)
          (message "Amalgamate, %s" collab-mode--pending-ops))
      ;; Didn’t amalgamate, send ops.
      (message "No amalgamate, %s %s"
               collab-mode--pending-ops
               collab-mode--ops-current-command)
      (collab-mode--send-ops (nreverse collab-mode--pending-ops))
      (setq collab-mode--pending-ops collab-mode--ops-current-command))

    (setq collab-mode--ops-current-command nil)

    ;; Register this buffer so the idle timer will send pending ops.
    (when (not (memq (current-buffer)
                     collab-mode--idle-timer-registered-buffer))
      (push (current-buffer) collab-mode--idle-timer-registered-buffer))))

(defun collab-mode--send-ops-now ()
  "Immediately send any pending ops to the collab process."
  (cl-assert (null collab-mode--ops-current-command))
  (message "Sending pending ops: %s" collab-mode--pending-ops)
  (let ((ops (nreverse collab-mode--pending-ops)))
    (collab-mode--send-ops ops)
    (setq collab-mode--pending-ops nil)))

(defun collab-mode--timer-send-ops ()
  "Send all pending ops to collab process."
  (dolist (buffer collab-mode--idle-timer-registered-buffer)
    (with-current-buffer buffer
      (collab-mode--send-ops-now)))
  (setq collab-mode--idle-timer-registered-buffer nil))

;; https://stackoverflow.com/questions/6590889
;; TODO: Support for overwrite-mode, abbrev-mode, auto-fill.
(defun collab-mode--maybe-amalgamate (op1 op2)
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

(defvar collab-mode--mode-line
  `(collab-monitored-mode
    ,(propertize "CONNECTED " 'face
                 '(:inherit (bold success))))
  "Mode-line indicator for ‘collab-monitored-mode’.")

(defvar collab-monitored-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map [remap undo-only] #'collab-mode-undo)
    (define-key map [remap undo-redo] #'collab-mode-redo)
    map)
  "Keymap for ‘collab-monitored-mode’.")

(defvar collab-mode--dispatcher-timer-table)
(define-minor-mode collab-monitored-mode
  "Collaborative monitor mode."
  :global nil
  :keymap collab-monitored-mode-map
  (if collab-monitored-mode
      (progn
        (add-hook 'change-major-mode-hook #'collab-mode--rescue-state 0 t)
        (add-hook 'after-change-major-mode-hook
                  #'collab-mode--maybe-recover 0)
        (add-hook 'before-change-functions
                  #'collab-mode--before-change 0 t)
        (add-hook 'after-change-functions #'collab-mode--after-change 0 t)
        (add-hook 'pre-command-hook #'collab-mode--pre-command 0 t)
        (if collab-mode-hasty-p
            (add-hook 'post-command-hook
                      #'collab-mode--post-command-hasty 0 t)
          (add-hook 'post-command-hook #'collab-mode--post-command 0 t))

        (unless collab-mode--edit-tracking-timer
          (setq collab-mode--edit-tracking-timer
                (run-with-idle-timer 1 t #'collab-mode--timer-send-ops)))

        (unless (member collab-mode--mode-line mode-line-misc-info)
          (setq-local mode-line-misc-info
                      (cons collab-mode--mode-line mode-line-misc-info)))

        (setq-local buffer-undo-list t)

        (setq-local collab-mode--pending-ops nil)
        (setq-local collab-mode--ops-current-command nil)
        (setq-local collab-mode--group-seq 1))

    ;; Disable.
    (remhash collab-mode--doc-server
             collab-mode--dispatcher-timer-table)

    (remove-hook 'before-change-functions #'collab-mode--before-change t)
    (remove-hook 'after-change-functions #'collab-mode--after-change t)
    (remove-hook 'pre-command-hook #'collab-mode--pre-command t)
    (remove-hook 'post-command-hook #'collab-mode--post-command t)
    (remove-hook 'post-command-hook #'collab-mode--post-command-hasty t)

    (remhash collab-mode--doc-server
             collab-mode--buffer-table)

    (dolist (ov collab-mode--cursor-ov-alist)
      (delete-overlay (cdr ov)))
    (setq-local collab-mode--cursor-ov-alist nil)

    (setq-local collab-mode--doc-server nil)
    (setq-local collab-mode--my-site-id nil)
    (setq-local buffer-undo-list nil)))

(defun collab-mode--enable (doc-id server-id my-site-id)
  "Enable ‘collab-monitored-mode’ in the current buffer.
DOC-ID and SERVER-ID are associated with the current buffer.
MY-SITE-ID is the site if of this editor."
  (setq collab-mode--doc-server (cons doc-id server-id))
  (setq collab-mode--my-site-id my-site-id)
  (puthash collab-mode--doc-server
           (current-buffer)
           collab-mode--buffer-table)
  (collab-monitored-mode)
  (let ((pulse-delay 0.1))
    (pulse-momentary-highlight-region
     (point-min) (point-max) 'diff-refine-added)))

(defun collab-mode--disable ()
  "Disable ‘collab-mode’ for the current buffer and flash red."
  (let ((pulse-delay 0.1))
    (pulse-momentary-highlight-region
     (point-min) (point-max) 'diff-refine-removed))
  (collab-monitored-mode -1))

(defvar collab-mode--stashed-state-plist nil
  "Used for stashing local state when major mode changes.")

(defun collab-mode--rescue-state ()
  "Stash local variable elsewhere before major mode changes."
  (setq collab-mode--stashed-state-plist
        (plist-put collab-mode--stashed-state-plist
                   (current-buffer)
                   collab-mode--doc-server)))

(defun collab-mode--maybe-recover ()
  "Maybe reenable ‘collab-mode’ after major mode change."
  (when-let ((doc-server (plist-get collab-mode--stashed-state-plist
                                    (current-buffer))))
    (collab-mode--enable (car doc-server) (cdr doc-server))))

(defun collab-mode--doc-connected (doc server)
  "Return non-nil if DOC at SERVER is connected."
  (let ((buf (gethash (cons doc server)
                      collab-mode--buffer-table)))
    (and buf (buffer-live-p buf))))

;;; JSON-RPC

;;;; Connection

(defvar collab-mode--jsonrpc-connection nil
  "The JSONRPC connection to the collab server.")

(defun collab-mode--connect ()
  "Get existing JSONRPC connection or create one."
  (or (and collab-mode--jsonrpc-connection
           (jsonrpc-running-p collab-mode--jsonrpc-connection)
           collab-mode--jsonrpc-connection)

      (let* ((process (pcase collab-mode-connection-type
                        (`(socket ,port)
                         (open-network-stream
                          "collab" nil "localhost" port))
                        ('pipe
                         (make-process
                          :name "collab"
                          :command (list collab-mode-command)
                          :connection-type 'pipe)))))

        (let ((conn (make-instance 'jsonrpc-process-connection
                                   :name "collab"
                                   :process process
                                   :notification-dispatcher
                                   #'collab-mode--dispatch-notification)))

          (when collab-mode--jsonrpc-connection
            (jsonrpc-shutdown collab-mode--jsonrpc-connection))
          (setq collab-mode--jsonrpc-connection conn)))))

;;;; Dispatcher

(defvar collab-mode--dispatcher-timer-table
  (make-hash-table :test #'equal)
  "Hash table mapping (doc-id . server-id) to dispatcher timers.")


(defun collab-mode--request-remote-ops-before-seq (doc server buffer seq)
  "Request for remote ops for (DOC SERVER) in BUFFER.
Keep requesting for ops until we get all the ops before and
including SEQ."
  (let ((received-largest-seq nil))
    (unwind-protect
        (with-current-buffer buffer
          (setq received-largest-seq (collab-mode--send-ops nil)))
      (let ((timer (if (or (null received-largest-seq)
                           (>= received-largest-seq seq))
                       nil
                     ;; If we didn’t get enough ops that we expect,
                     ;; schedule another fetch.
                     (run-with-timer
                      0 nil
                      #'collab-mode--request-remote-ops-before-seq
                      doc server buffer seq))))
        (puthash (cons doc server) timer
                 collab-mode--dispatcher-timer-table)))))

(defun collab-mode--dispatch-notification (_conn method params)
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
                              collab-mode--buffer-table))
             (timer (gethash (cons doc server)
                             collab-mode--dispatcher-timer-table)))
         (when timer (cancel-timer timer))
         (when (buffer-live-p buffer)
           (setq timer (run-with-timer
                        0 nil
                        #'collab-mode--request-remote-ops-before-seq
                        doc server buffer last-seq))
           (puthash (cons doc server) timer
                    collab-mode--dispatcher-timer-table)))))
    ('ServerError
     (display-warning
      'collab-mode
      (format "Local server errored, you might want to restart collab process. Cause: %s"
              params)))
    ('SignalingTimesUp
     (run-with-timer
      0 nil
      (lambda ()
        (with-current-buffer collab-mode--hub-buffer
          (setq collab-mode--accepting-connection nil)
          (collab-mode--refresh)
          (message "Collab process stopped accepting connections")))))
    ('AcceptConnectionErr
     (run-with-timer
      0 nil
      (lambda ()
        (with-current-buffer collab-mode--hub-buffer
          (setq collab-mode--accepting-connection nil)
          (collab-mode--refresh)
          (display-warning
           'collab-mode
           (format "Error accepting remote peer connections: %s" params))))))))

;;;; Requests

(defun collab-mode--list-files-req (server signaling-server credential)
  "Return a list of files on SERVER (address) with CREDENTIAL.
SIGNALING-SERVER is the address of the signaling server. Each
file is of the form (:docId DOC-ID :fileName FILE-NAME)."
  (let* ((conn (collab-mode--connect))
         (resp (jsonrpc-request
                conn 'ListFiles
                `( :serverId ,server
                   :signalingAddr ,signaling-server
                   :credential ,credential)
                :timeout collab-mode-rpc-timeout)))
    (seq-map #'identity (plist-get resp :files))))

(defun collab-mode--share-file-req (server filename content)
  "Share the file with SERVER (address).
FILENAME is filename, CONTENT is just the content of the file in
a string. Return (:docId DOC-ID :siteId SITE-ID). If FORCE is
non-nil, override existing files."
  (let* ((conn (collab-mode--connect))
         (resp (jsonrpc-request
                conn 'ShareFile
                `( :serverId ,server
                   :fileName ,filename
                   :content ,content)
                :timeout collab-mode-rpc-timeout)))
    resp))

(defun collab-mode--connect-to-file-req (doc server)
  "Connect to DOC (doc id) on SERVER (server address).
Return (:siteId SITE-ID :content CONTENT)."
  (let* ((conn (collab-mode--connect))
         (resp (jsonrpc-request
                conn 'ConnectToFile
                `( :serverId ,server
                   :docId ,doc)
                :timeout collab-mode-rpc-timeout)))
    resp))

(defun collab-mode--disconnect-from-file-req (doc server)
  "Disconnect from DOC on SERVER."
  (let ((conn (collab-mode--connect)))
    (jsonrpc-request conn 'DisconnectFromFile
                     `( :serverId ,server
                        :docId ,doc)
                     :timeout collab-mode-rpc-timeout)))

(defun collab-mode--delete-file-req (doc server)
  "Delete DOC on SERVER."
  (let ((conn (collab-mode--connect)))
    (jsonrpc-request conn 'DeleteFile
                     `( :serverId ,server
                        :docId ,doc)
                     :timeout collab-mode-rpc-timeout)))

(defun collab-mode--accept-connection-req (server-id signaling-addr)
  "Accept connections as SERVER-ID on SIGNALING-ADDR."
  (let ((conn (collab-mode--connect)))
    (jsonrpc-request conn 'AcceptConnection
                     `( :serverId ,server-id
                        :signalingAddr ,signaling-addr)
                     :timeout collab-mode-rpc-timeout)))

(defun collab-mode--print-history-req (doc server debug)
  "Print debugging history for (DOC SERVER).
If DEBUG non-nil, print debug version."
  (let ((conn (collab-mode--connect)))
    (jsonrpc-request conn 'PrintHistory
                     `( :docId ,doc
                        :serverId ,server
                        :debug ,(if debug t :json-false))
                     :timeout collab-mode-rpc-timeout)))

(defsubst collab-mode--encode-op (op)
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

(defsubst collab-mode--apply-jrpc-op (op &optional move-point)
  "Decode JSPNRPC OP into Elisp format.
If MOVE-POINT non-nil, move point as the edit would."
  (let ((inhibit-modification-hooks t)
        (collab-mode--inhibit-hooks t)
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

(defun collab-mode--send-ops (ops &optional encoded)
  "Send OPS to the collab server and apply returned remote ops.

If ENCODED is non-nil, OPS should be already in sexp JSON
format (a list of EditorOp), and this function will skip encoding
it.

Return the largest global seq of all the ops received from the
collab process."
  (collab-mode--check-precondition)
  (let ((conn collab-mode--jsonrpc-connection)
        resp)
    (collab-mode--catch-error "can’t send local edits to remote"
      ;; RESP := (:op LEAN-OP :siteId SITE-ID)
      ;; LEAN-OP := (:Ins [POS STR]) | (:Del [[POS STR]])
      (setq resp
            (jsonrpc-request
             conn 'SendOp
             `( :docId ,(car collab-mode--doc-server)
                :serverId ,(cdr collab-mode--doc-server)
                :ops ,(if (not encoded)
                          (if ops
                              (vconcat (mapcar #'collab-mode--encode-op
                                               ops))
                            [])
                        ops))
             :timeout collab-mode-rpc-timeout))
      ;; Only ‘seq-map’ can map over vector.
      (let ((remote-ops (plist-get resp :ops))
            (last-seq (plist-get resp :lastSeq))
            last-op)
        (seq-map (lambda (op)
                   (collab-mode--apply-jrpc-op op)
                   (setq last-op op))
                 remote-ops)
        (pcase last-op
          (`(:op (:Ins [,pos ,str]) :siteId ,site-id)
           (collab-mode--move-cursor site-id (+ 1 pos (length str))))
          (`(:op (:Del ,edits) :siteId ,site-id)
           (when (> (length edits) 0)
             (collab-mode--move-cursor
              site-id (1+ (aref (aref edits 0) 0))))))
        ;; Return the largest global seq received from collab process.
        last-seq))))

(defun collab-mode--undo (&optional redo)
  "Undo the most recent local edit.
If REDO is non-nil, redo the most recent undo instead."
  (interactive)
  (collab-mode--check-precondition)
  (let ((conn collab-mode--jsonrpc-connection)
        resp)

    (collab-mode--send-ops-now)

    (collab-mode--catch-error "can’t undo/redo"
      (setq resp
            (jsonrpc-request conn 'Undo
                             `( :serverId ,(cdr collab-mode--doc-server)
                                :docId ,(car collab-mode--doc-server)
                                :kind ,(if redo "Redo" "Undo"))
                             :timeout collab-mode-rpc-timeout))
      (if-let ((json-ops (plist-get resp :ops)))
          (if (eq (length json-ops) 0)
              (message "No more operations to %s" (if redo "redo" "undo"))
            ;; Only ‘seq-map’ can map over vector. TODO: if the op is
            ;; identify, undo one more step.
            (seq-map (lambda (json-op)
                       (collab-mode--apply-jrpc-op json-op t))
                     json-ops)
            (collab-mode--send-ops
             (vconcat (seq-map (lambda (json-op)
                                 `( :op ,json-op
                                    :groupSeq ,collab-mode--group-seq
                                    :kind ,(if redo "Redo" "Undo")))
                               json-ops))
             t))
        (user-error "No more operations to %s"
                    (if redo "redo" "undo"))))))

(defun collab-mode-undo ()
  "Undo the most recent local edit."
  (interactive)
  (collab-mode--undo))

(defun collab-mode-redo ()
  "Redo the most recent undo operation."
  (interactive)
  (collab-mode--undo t))

;;; UI

(defvar collab-mode--hub-buffer "*collab*"
  "Buffer name for the hub buffer.")

(defmacro collab-mode--save-excursion (&rest body)
  "Save position, execute BODY, and restore point.
Point is not restored in the face of non-local exit."
  `(let ((pos (point-marker)))
     ,@body
     (goto-char pos)))

(defface collab-mode-dynamic-highlight
  '((t . (:background "gray90" :extend t)))
  "Face used for the dynamic highlight in the collab buffer.")

(defface collab-mode-server '((t . (:inherit bold)))
  "Face used for the server line in the collab buffer.")

(defface collab-mode-file '((t . (:inherit default)))
  "Face used for files in the collab buffer.")

;;;; Drawing the hub UI

(defun collab-mode--insert-file (doc-id file-name server)
  "Insert file that has DOC-ID and FILE-NAME.
SERVER is its server id."
  (let ((beg (point)))
    (insert file-name)
    (add-text-properties
     beg (point) `( collab-mode-doc-id ,doc-id
                    collab-mode-file-name ,file-name))
    (when (collab-mode--doc-connected doc-id server)
      (insert " " (propertize (icon-string 'collab-status-on)
                              'collab-mode-status t)))))

(defun collab-mode--insert-server (server signaling-server credential)
  "Insert SERVER (server id) on SIGNALING-SERVER and its files.
Server has CREDENTIAL. SIGNALING-SERVER is the address of the
signaling server."
  (let ((beg (point)))
    ;; 1. Insert server line.
    (insert (propertize server 'face 'bold
                        'collab-mode-server-line t))
    (unless (equal server "self")
      (insert (propertize " UP" 'face 'success)))
    (insert (propertize "\n" 'line-spacing 0.4))
    (let ((files (collab-mode--list-files-req
                  server signaling-server credential)))
      ;; 2. Insert files.
      (if files
          (dolist (file files)
            (let ((doc-id (plist-get file :docId))
                  (file-name (plist-get file :fileName)))
              (collab-mode--insert-file doc-id file-name server)
              (insert "\n")))
        (insert (propertize "(empty)\n" 'face 'shadow)))
      ;; 4. Mark server section.
      (add-text-properties beg (point)
                           `(collab-mode-server-id ,server)))))

(defun collab-mode--insert-disconnected-server
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
                         `(collab-mode-server-id ,server))))

(defun collab-mode--prop-section (prop)
  "Return the (BEG . END) of the range that has PROP."
  (save-excursion
    (let ((forward-prop (text-property-search-forward prop))
          (backward-prop (text-property-search-backward prop)))
      (cons (prop-match-beginning backward-prop)
            (prop-match-end forward-prop)))))

(defvar collab--hub-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "RET") #'collab-mode--open-file)
    (define-key map (kbd "x") #'collab-mode--delete-file)
    (define-key map (kbd "k") #'collab-mode--disconnect-from-file)
    (define-key map (kbd "g") #'collab-mode--refresh)
    (define-key map (kbd "A") #'collab-mode--accept-connection)

    (define-key map (kbd "n") #'next-line)
    (define-key map (kbd "p") #'previous-line)
    map)
  "Keymap for ‘collab--hub-mode’.")

(define-derived-mode collab--hub-mode special-mode "Collab"
  "Collaboration mode."
  (setq-local line-spacing 0.2
              eldoc-idle-delay 0.8)
  (add-hook 'eldoc-documentation-functions #'collab-mode--eldoc 0 t)
  (add-hook 'post-command-hook #'collab-mode--dynamic-highlight)
  (eldoc-mode))

(defun collab-mode--render ()
  "Render collab mode buffer."
  (let ((connection-up nil))
    ;; Insert headers.
    (insert "Connection: ")
    (condition-case err
        (progn
          (collab-mode--connect)
          (insert (propertize "UP" 'face 'success) "\n")
          (setq connection-up t))
      (error
       (insert (propertize "DOWN" 'face 'error) "\n")
       (setq collab-mode--most-recent-error
             (format "Cannot connect to collab process: %s"
                     (error-message-string err)))))
    (insert (format "Connection type: %s\n" collab-mode-connection-type))
    (insert "Accepting remote peer connections: "
            (if collab-mode--accepting-connection
                (propertize "YES" 'face 'success)
              (propertize "NO" 'face 'shadow))
            "\n")
    (insert "Errors: ")
    (insert "0" "\n")
    (insert "\n")

    ;; Insert each server and files.
    (dolist (entry (collab-mode--server-alist))
      (let ((beg (point)))
        (condition-case err
            (if connection-up
                (collab-mode--insert-server
                 (car entry) (nth 0 (cdr entry)) (nth 1 (cdr entry)))
              (collab-mode--insert-disconnected-server (car entry)))
          (jsonrpc-error
           (delete-region beg (point))
           (collab-mode--insert-disconnected-server (car entry) t)
           (setq collab-mode--most-recent-error
                 (format "Error connecting to remote peer: %s" err)))))
      ;; Insert an empty line after a server section.
      (insert "\n"))

    (insert "PRESS + TO CONNECT to REMOTE SERVER\n")
    (insert "PRESS A TO ACCEPT REMOTE CONNECTIONS (for 180s)\n")
    (when collab-mode--most-recent-error
      (insert
       (propertize (concat "\n\nMost recent error:\n\n"
                           collab-mode--most-recent-error
                           "\n")
                   'face 'shadow))
      (setq collab-mode--most-recent-error nil))))

;;;; Dynamic highlight

(defvar-local collab-mode--hl-ov-1 nil
  "Overlay used for 1st level highlight.")

(defvar-local collab-mode--hl-ov-2 nil
  "Overlay used for 2nd level highlight.")

(defun collab-mode--dynamic-highlight ()
  "Highlight server and file at point."
  (let ((ov (or collab-mode--hl-ov-1
                (let ((ov (make-overlay 1 1)))
                  (overlay-put ov 'face 'collab-mode-dynamic-highlight)
                  (setq collab-mode--hl-ov-1 ov)))))
    (cond
     ((get-text-property (point) 'collab-mode-doc-id)
      (let ((range (collab-mode--prop-section 'collab-mode-doc-id)))
        (move-overlay ov (car range) (cdr range)))
      (setq-local cursor-type nil))
     ((get-text-property (point) 'collab-mode-server-line)
      (let ((range (collab-mode--prop-section 'collab-mode-server-id)))
        (move-overlay ov (car range) (cdr range)))
      (setq-local cursor-type nil))
     (t
      (when ov
        (move-overlay ov 1 1))
      (kill-local-variable 'cursor-type)))))

;;;; Eldoc

(defun collab-mode--eldoc (callback)
  "Displays help information at point.
Used for ‘eldoc-documentation-functions’, calls CALLBACK
immediately."
  (let ((doc-id (get-text-property (point) 'collab-mode-doc-id))
        (server-id (get-text-property (point) 'collab-mode-server-id)))
    (cond
     (doc-id
      (funcall callback
               "RET → Open File  x → Delete File  k → Disconnect"))
     (server-id
      (funcall callback "s → Share File"))
     (t
      (funcall callback "g → Refresh")))))

;;;; Interactive commands

(defun collab-mode-shutdown ()
  "Shutdown the connection to the collab process."
  (interactive)
  (when collab-mode--jsonrpc-connection
    (jsonrpc-shutdown collab-mode--jsonrpc-connection)
    (setq collab-mode--jsonrpc-connection nil)))

(defun collab-mode--refresh ()
  "Refresh collab buffer."
  (interactive)
  (let ((inhibit-read-only t)
        (line (line-number-at-pos)))
    (erase-buffer)
    (collab-mode--render)
    (goto-char (point-min))
    (forward-line (1- line))))

(defun collab-mode--open-file ()
  "Open the file at point."
  (interactive)
  (let* ((doc-id (get-text-property (point) 'collab-mode-doc-id))
         (file-name (get-text-property (point) 'collab-mode-file-name))
         (server-id (get-text-property (point) 'collab-mode-server-id))
         (buf (gethash (cons doc-id server-id)
                       collab-mode--buffer-table)))
    (if (and buf (buffer-live-p buf))
        (pop-to-buffer buf)
      (collab-mode--catch-error (format "can’t connect to Doc(%s)" doc-id)
        (let* ((resp (collab-mode--connect-to-file-req doc-id server-id))
               (site-id (plist-get resp :siteId))
               (content (plist-get resp :content))
               (inhibit-read-only t))
          (end-of-line)
          (unless (get-text-property (1- (point)) 'collab-mode-status)
            (insert " " (propertize (icon-string 'collab-status-on)
                                    'collab-mode-status t)))
          (pop-to-buffer
           (generate-new-buffer (concat "*collab: " file-name "*")))
          (collab-monitored-mode -1)
          (erase-buffer)
          (insert content)
          (goto-char (point-min))
          (let ((buffer-file-name file-name))
            (set-auto-mode))
          (collab-mode--enable doc-id server-id site-id))))))

(defun collab-mode--disconnect-from-file ()
  "Disconnect from the file at point."
  (interactive)
  (let ((doc-id (get-text-property (point) 'collab-mode-doc-id))
        (server-id (get-text-property (point) 'collab-mode-server-id)))
    (collab-mode--catch-error
        (format "can’t disconnect from Doc(%s)" doc-id)
      (collab-mode--disconnect-from-file-req doc-id server-id))
    (let ((buf (gethash (cons doc-id server-id)
                        collab-mode--buffer-table))
          (inhibit-read-only t))
      (when buf
        (with-current-buffer buf
          (collab-mode--disable)))
      (save-excursion
        (end-of-line)
        (when (looking-back " •" 2)
          (delete-char -2))))))

(defun collab-mode--delete-file ()
  "Delete the file at point."
  (interactive)
  (let ((doc-id (get-text-property (point) 'collab-mode-doc-id))
        (server-id (get-text-property (point) 'collab-mode-server-id)))
    (collab-mode--catch-error (format "can’t delete Doc(%s)" doc-id)
      (collab-mode--delete-file-req doc-id server-id))
    (collab-mode--refresh)))

(defun collab-mode--accept-connection ()
  "Start accepting connections."
  (interactive)
  (let ((server-id (nth 0 collab-mode-local-server-config))
        (signaling-addr (nth 1 collab-mode-local-server-config)))
    (collab-mode--catch-error "can’t accept connection "
      (collab-mode--accept-connection-req server-id signaling-addr))
    (setq-local collab-mode--accepting-connection t)
    (collab-mode--refresh)))

(defun collab-hub ()
  "Pop up the collab hub interface."
  (interactive)
  (switch-to-buffer (get-buffer-create collab-mode--hub-buffer))
  (collab--hub-mode)
  (collab-mode--refresh))

(defun collab-mode-share-buffer (server file-name)
  "Share the current buffer to SERVER under FILE-NAME.
When called interactively, prompt for the server."
  (interactive (list (completing-read
                      "Server: "
                      (mapcar #'car (collab-mode--server-alist)))
                     (read-string
                      "File name: "
                      (or (and buffer-file-name
                               (file-name-nondirectory buffer-file-name))
                          (buffer-name)))))
  (collab-mode--catch-error "can’t share the current buffer"
    (let* ((resp (collab-mode--share-file-req
                  server file-name
                  (buffer-substring-no-properties (point-min) (point-max))))
           (doc-id (plist-get resp :docId))
           (site-id (plist-get resp :siteId)))
      (collab-mode--enable doc-id server site-id))))

(defun collab-mode-reconnect-buffer (server doc-id)
  "Reconnect current buffer to a remote document SERVER.

Reconnecting will override the current content of the buffer.

The document to reconnect to is determined by DOC-ID. But if
called interactively, prompt the user to select a document (by
its name rather than doc id) to connect."
  (interactive (list (completing-read
                      "Server: "
                      (mapcar #'car (collab-mode--server-alist)))
                     'interactive))
  (collab-mode--catch-error (format "can’t reconnect to Doc(%s)" doc-id)
    (let* ((info (alist-get server (collab-mode--server-alist)
                            nil nil #'equal))
           (file-list (collab-mode--list-files-req
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
      (let* ((resp (collab-mode--connect-to-file-req doc-id server))
             (content (plist-get resp :content))
             (site-id (plist-get resp :siteId))
             (inhibit-read-only t))

        (when (buffer-modified-p)
          (when (y-or-n-p "Save buffer before merging?")
            (save-buffer)))

        ;; Same as in ‘collab-mode--open-file’.
        (collab-monitored-mode -1)
        (erase-buffer)
        (insert content)
        (goto-char (point-min))
        (let ((buffer-file-name file-name))
          (set-auto-mode))
        (collab-mode--enable doc-id server site-id)))))

(defun collab-mode-disconnect-buffer ()
  "Disconnect current buffer, returning it to a regular buffer."
  (interactive)
  (collab-mode--check-precondition)
  (let ((doc-id (car collab-mode--doc-server))
        (server-id (cdr collab-mode--doc-server)))
    (collab-mode--catch-error
        (format "can’t disconnect from Doc(%s)" doc-id)
      (collab-mode--disconnect-from-file-req doc-id server-id)))
  (collab-mode--disable)
  (message "Disconnected"))

(defun collab-mode--print-history (&optional debug)
  "Print debugging history for the current buffer.
If called with an interactive argument (DEBUG), print more
detailed history."
  (interactive "p")
  (collab-mode--check-precondition)
  (let ((doc-id (car collab-mode--doc-server))
        (server-id (cdr collab-mode--doc-server))
        (debug (eq debug 4)))
    (collab-mode--catch-error
        (format "can’t print history of Doc(%s)" doc-id)
      (collab-mode--send-ops-now)
      (let ((text (collab-mode--print-history-req doc-id server-id debug))
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
      (?s (call-interactively #'collab-mode-share-buffer))
      (?r (call-interactively #'collab-mode-reconnect-buffer))
      (?d (collab-mode-disconnect-buffer))
      (?h (collab-hub)))))

;;; Debug

(defun collab-mode--setup-2 ()
  "Setup test session #2."
  (interactive)
  (setq collab-mode-server-alist '(("#1" . ("ws://127.0.0.1:8822" "blah"))))
  (setq collab-mode-local-server-config '("#2" "ws://127.0.0.1:8822"))
  (setq collab-mode-connection-type '(socket 7702)))

(provide 'collab-mode)

;;; collab-mode.el ends here
