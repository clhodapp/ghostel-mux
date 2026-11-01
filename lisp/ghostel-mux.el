;;; ghostel-mux.el --- Ghostel terminals that outlive Emacs -*- lexical-binding: t; -*-

;; Copyright (C) 2026 Chris Hodapp
;; SPDX-License-Identifier: GPL-3.0-or-later
;; Package-Requires: ((emacs "30.1") (ghostel "0.60.0"))

;; This program is free software: you can redistribute it and/or modify
;; it under the terms of the GNU General Public License as published by
;; the Free Software Foundation, either version 3 of the License, or
;; (at your option) any later version.
;;
;; This program is distributed in the hope that it will be useful,
;; but WITHOUT ANY WARRANTY; without even the implied warranty of
;; MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
;; GNU General Public License for more details.
;;
;; You should have received a copy of the GNU General Public License
;; along with this program.  If not, see <https://www.gnu.org/licenses/>.

;;; Commentary:

;; With `ghostel-mux-mode' on, each local ghostel terminal runs its shell
;; inside a ghostel-mux session process instead of a PTY that Emacs owns.
;; The session keeps the shell and a copy of the terminal state, so the
;; terminal survives Emacs exiting, crashing or being replaced, and another
;; Emacs can adopt it later with its screen and scrollback intact.
;;
;; A session has at most one owning Emacs.  `ghostel-mux-list' shows the
;; sessions of every Emacs this user runs; from there a session can be
;; adopted, taken from the Emacs that owns it, forced off its owner, or
;; killed.  `ghostel-mux-detach' lets go of the current terminal and leaves
;; its session running.
;;
;; Requires a ghostel whose native module has `ghostel--new-from-snapshot'
;; and `ghostel--connect-native-socket'.  Remote (TRAMP) terminals are not
;; affected.

;;; Code:

(require 'ghostel)
(require 'cl-lib)
(require 'tabulated-list)

(defgroup ghostel-mux nil
  "Ghostel terminals that outlive Emacs."
  :group 'ghostel)

(defcustom ghostel-mux-program "ghostel-mux"
  "The ghostel-mux executable."
  :type 'string)

(defcustom ghostel-mux-timeout 10
  "Seconds to wait for a session to answer a request."
  :type 'number)

(defcustom ghostel-mux-adopt-on-startup nil
  "Whether enabling `ghostel-mux-mode' adopts every detached session.
Sessions whose command has exited are left for `ghostel-mux-list'."
  :type 'boolean)

(defvar-local ghostel-mux--id nil
  "Id of the session behind this ghostel buffer.")

(defvar-local ghostel-mux--ctl nil
  "The control connection to this buffer's session.")

(defvar-local ghostel-mux--exit-status nil
  "Exit status of the session's command, once the session reports it.")

(defvar-local ghostel-mux--lost nil
  "Why this buffer lost its session, when another client took it.")

(defvar-local ghostel-mux--detaching nil
  "Non-nil while this buffer is being killed without ending its session.")

(defvar ghostel-mux--pending-snapshot nil
  "Snapshot bytes for the next `ghostel--new' to restore, when non-nil.")

;;; Paths and identity

(defun ghostel-mux--root ()
  "Directory holding one subdirectory per session."
  (expand-file-name "ghostel-mux"
                    (or (getenv "XDG_RUNTIME_DIR")
                        (format "/tmp/ghostel-mux-%d" (user-uid)))))

(defun ghostel-mux--socket (id name)
  "Path of socket NAME (\"ctl\" or \"data\") of session ID."
  (expand-file-name name (expand-file-name id (ghostel-mux--root))))

(defun ghostel-mux--client-name ()
  "How this Emacs names itself to sessions."
  (format "%s:%d@%s"
          (if (daemonp) (format "emacs-daemon(%s)" server-name) "emacs")
          (emacs-pid)
          (system-name)))

(defun ghostel-mux--run (&rest args)
  "Run `ghostel-mux-program' with ARGS and return its standard output.
Signals an error, with the program's message, when it fails."
  (with-temp-buffer
    (let ((status (apply #'call-process ghostel-mux-program nil (list t t) nil args)))
      (unless (eql status 0)
        (error "ghostel-mux %s: %s" (car args) (string-trim (buffer-string))))
      (buffer-string))))

(defun ghostel-mux--sessions ()
  "Metadata of every session, as a list of alists."
  (append (json-parse-string (ghostel-mux--run "list" "--json")
                             :object-type 'alist :array-type 'list
                             :null-object nil :false-object nil)
          nil))

;;; The control connection

(defun ghostel-mux--connect (id)
  "Open a control connection to session ID."
  (let ((proc (make-network-process
               :name (format "ghostel-mux-ctl-%s" id)
               :family 'local
               :service (ghostel-mux--socket id "ctl")
               :buffer (generate-new-buffer (format " *ghostel-mux-ctl %s*" id))
               :coding 'binary
               :noquery t
               :filter #'ghostel-mux--ctl-filter
               :sentinel #'ghostel-mux--ctl-sentinel)))
    (with-current-buffer (process-buffer proc)
      (set-buffer-multibyte nil))
    proc))

(defun ghostel-mux--send (proc request)
  "Send REQUEST, an alist, to the session at PROC."
  (process-send-string proc (concat (json-serialize request) "\n")))

(defun ghostel-mux--ctl-filter (proc output)
  "Parse events in OUTPUT from the session at PROC."
  (with-current-buffer (process-buffer proc)
    (goto-char (point-max))
    (insert output)
    (goto-char (point-min))
    (catch 'incomplete
      (while (< (point) (point-max))
        (let ((need (process-get proc 'ghostel-mux-snapshot)))
          (if need
              (if (< (- (point-max) (point)) need)
                  (throw 'incomplete nil)
                (let ((bytes (buffer-substring-no-properties (point) (+ (point) need))))
                  (delete-region (point-min) (+ (point) need))
                  (process-put proc 'ghostel-mux-snapshot nil)
                  (ghostel-mux--event proc `((type . "snapshot") (bytes . ,bytes)))))
            (let ((eol (search-forward "\n" nil t)))
              (unless eol (throw 'incomplete nil))
              (let ((event (json-parse-string
                            (decode-coding-string
                             (buffer-substring-no-properties (point-min) (1- eol)) 'utf-8)
                            :object-type 'alist :array-type 'list
                            :null-object nil :false-object nil)))
                (delete-region (point-min) eol)
                (if (equal (alist-get 'type event) "snapshot")
                    (process-put proc 'ghostel-mux-snapshot (alist-get 'bytes event))
                  (ghostel-mux--event proc event))))))))))

(defun ghostel-mux--event (proc event)
  "Handle EVENT from the session at PROC.
Events for an attached buffer are acted on there; the rest wait in a
queue for `ghostel-mux--await'."
  (let ((buffer (process-get proc 'ghostel-mux-buffer))
        (type (alist-get 'type event)))
    (if (and buffer (member type '("exit" "stolen" "evicted")))
        (when (buffer-live-p buffer)
          (with-current-buffer buffer
            (pcase type
              ("exit"
               (setq ghostel-mux--exit-status (alist-get 'status event))
               (ghostel-mux--send proc '((op . "exit-ack"))))
              ("stolen"
               (setq ghostel-mux--lost (format "taken by %s" (alist-get 'by event))))
              ("evicted"
               (setq ghostel-mux--lost "detached by another client")))))
      (process-put proc 'ghostel-mux-events
                   (append (process-get proc 'ghostel-mux-events) (list event))))))

(defun ghostel-mux--ctl-sentinel (proc _event)
  "Clean up after the control connection PROC closes."
  (unless (process-live-p proc)
    (when (buffer-live-p (process-buffer proc))
      (kill-buffer (process-buffer proc)))))

(defun ghostel-mux--await (proc &rest types)
  "Wait for an event of one of TYPES from the session at PROC and return it.
An `error' event signals an error."
  (let ((deadline (+ (float-time) ghostel-mux-timeout))
        found)
    (while (not found)
      (let ((events (process-get proc 'ghostel-mux-events)))
        (when-let* ((err (cl-find "error" events :key (lambda (e) (alist-get 'type e)) :test #'equal)))
          (process-put proc 'ghostel-mux-events (delq err events))
          (error "ghostel-mux: %s" (alist-get 'message err)))
        (setq found (cl-find-if (lambda (e) (member (alist-get 'type e) types)) events))
        (if found
            (process-put proc 'ghostel-mux-events (delq found events))
          (unless (process-live-p proc)
            (error "ghostel-mux: the session closed the connection"))
          (when (> (float-time) deadline)
            (error "ghostel-mux: the session did not answer"))
          (accept-process-output proc 0.05))))
    found))

;;; Attaching a buffer to a session

(defun ghostel-mux--size-request ()
  "The current buffer's terminal size, as request fields."
  `((rows . ,(or ghostel--term-rows 24))
    (cols . ,(or ghostel--term-cols 80))
    (cell_width . ,(ghostel--reported-cell-width))
    (cell_height . ,(round (* (ghostel--cell-height) (ghostel--cell-pixel-scale))))))

(defun ghostel-mux--attach-buffer (id ctl info)
  "Connect the current ghostel buffer to session ID.
CTL is the control connection, on which this Emacs now owns the
session, and INFO the session's metadata.  Returns the pipe process
that stands for the terminal's process."
  (let ((pipe (make-pipe-process :name (format "ghostel-mux-%s" id)
                                 :buffer (current-buffer)
                                 :filter #'ghostel--events-filter
                                 :sentinel #'ghostel-mux--pipe-sentinel
                                 :noquery t)))
    (process-put ctl 'ghostel-mux-buffer (current-buffer))
    (process-put pipe 'adjust-window-size-function #'ignore)
    (setq ghostel-mux--id id
          ghostel-mux--ctl ctl
          ghostel-mux--exit-status nil
          ghostel-mux--lost nil
          ghostel--pid (alist-get 'pid info)
          ghostel--process pipe)
    (ghostel--connect-native-socket ghostel--term (ghostel-mux--socket id "data") pipe
                                    (alist-get 'tty info))
    (add-hook 'kill-buffer-hook #'ghostel-mux--kill-buffer-hook nil t)
    (ghostel-mux--send ctl `((op . "resize") ,@(ghostel-mux--size-request)))
    (ghostel--sync-read-only)
    pipe))

(defun ghostel-mux--pipe-sentinel (process event)
  "Finish the ghostel buffer whose session connection PROCESS closed.
EVENT is the state change Emacs reports."
  (let ((buffer (process-buffer process)))
    (when (buffer-live-p buffer)
      (with-current-buffer buffer
        (cond
         (ghostel-mux--lost
          (ghostel--write-vt ghostel--term
                             (format "\e[!p\r\n[ghostel-mux: session %s %s]\r\n"
                                     ghostel-mux--id ghostel-mux--lost))
          (setq ghostel--process nil)
          (ghostel--sync-read-only)
          (ghostel--invalidate))
         (t
          (ghostel--sentinel
           process
           (pcase ghostel-mux--exit-status
             ('nil event)
             (0 "finished\n")
             (code (format "exited abnormally with code %d\n" code))))))))))

(defun ghostel-mux--kill-buffer-hook ()
  "End the session with its buffer, unless the buffer is detaching."
  (let ((ctl ghostel-mux--ctl))
    (when (process-live-p ctl)
      ;; A session that reported an exit is already ending and may have
      ;; closed its end, so there is nothing to hang up.
      (unless (or ghostel-mux--detaching ghostel-mux--exit-status)
        (ignore-errors (ghostel-mux--send ctl '((op . "kill")))))
      (delete-process ctl)))
  (when ghostel--process
    (set-process-buffer ghostel--process nil))
  (when ghostel--term
    (ghostel--kill-native-process ghostel--term)))

;;; Starting a terminal in a session

(defun ghostel-mux--environment ()
  "The environment `process-environment' describes, one VAR=VALUE each.
An earlier entry shadows a later one, and an entry without `=' unsets
its variable."
  (let ((seen (make-hash-table :test #'equal))
        env)
    (dolist (entry process-environment)
      (let* ((pos (string-search "=" entry))
             (name (if pos (substring entry 0 pos) entry)))
        (unless (gethash name seen)
          (puthash name t seen)
          (when pos (push entry env)))))
    (when (and (not (gethash "DISPLAY" seen)) (display-graphic-p))
      (when-let* ((display (getenv "DISPLAY")))
        (push (concat "DISPLAY=" display) env)))
    (nreverse env)))

(defun ghostel-mux--spawn (argv)
  "Run ARGV in a new session attached to the current ghostel buffer.
Returns the pipe process that stands for the terminal's process."
  (let* ((id (string-trim (ghostel-mux--run "start")))
         (ctl (ghostel-mux--connect id)))
    (condition-case err
        (progn
          (ghostel-mux--send
           ctl `((op . "spawn")
                 (client . ,(ghostel-mux--client-name))
                 (argv . ,(vconcat argv))
                 (env . ,(vconcat (ghostel-mux--environment)))
                 (cwd . ,(expand-file-name default-directory))
                 (scrollback . ,ghostel-max-scrollback)
                 (kitty_storage_limit . ,ghostel-kitty-graphics-storage-limit)
                 (kitty_mediums . ,(ghostel--kitty-mediums-bits))
                 (kitty_temp_dir . ,(expand-file-name temporary-file-directory))
                 (name . ,(buffer-name))
                 ,@(ghostel-mux--size-request)))
          (ghostel-mux--attach-buffer
           id ctl (alist-get 'info (ghostel-mux--await ctl "attached"))))
      (error
       (delete-process ctl)
       (signal (car err) (cdr err))))))

(defun ghostel-mux--spawn-process (orig program program-args remote-p)
  "Run PROGRAM with PROGRAM-ARGS in a session unless REMOTE-P.
Around advice for `ghostel--spawn-process'; ORIG is the original
function."
  (if remote-p
      (funcall orig program program-args remote-p)
    (let ((process (ghostel-mux--spawn
                    (cons (ghostel--resolve-local-executable program) program-args))))
      (setq ghostel--process process)
      (ghostel--sync-read-only)
      process)))

(defun ghostel-mux--new (orig &rest args)
  "Build the terminal from `ghostel-mux--pending-snapshot' when it is set.
Around advice for `ghostel--new'; ORIG is the original function and
ARGS its arguments."
  (if-let* ((snapshot ghostel-mux--pending-snapshot))
      (ghostel--new-from-snapshot snapshot (nth 3 args) (nth 4 args))
    (apply orig args)))

(defun ghostel-mux--snapshot-error (snapshot)
  "Return why SNAPSHOT cannot be restored, or nil when it can.
A session writes its snapshot with the libghostty it was built with,
and a session started before an upgrade can meet an Emacs whose
ghostel uses another one."
  (with-temp-buffer
    (condition-case err
        (progn (ghostel--new-from-snapshot snapshot) nil)
      (error (error-message-string err)))))

(defun ghostel-mux--after-set-size (term rows cols &optional cell-width cell-height)
  "Tell the session about the new size of TERM.
After advice for `ghostel--set-size' with its arguments ROWS, COLS,
CELL-WIDTH and CELL-HEIGHT."
  (when (and ghostel-mux--ctl (eq term ghostel--term) (process-live-p ghostel-mux--ctl))
    (ghostel-mux--send ghostel-mux--ctl
                       `((op . "resize") (rows . ,rows) (cols . ,cols)
                         (cell_width . ,(or cell-width 0))
                         (cell_height . ,(or cell-height 0))))))

(defun ghostel-mux--foreground-pid (orig)
  "Return the foreground process group of a session's terminal.
Around advice for `ghostel-foreground-pid'; ORIG is the original
function.  Emacs holds no file descriptor for the session's PTY, so the
group comes from the /proc entry of the shell."
  (if (and ghostel-mux--id ghostel--pid)
      (ghostel-mux--tpgid ghostel--pid)
    (funcall orig)))

(defun ghostel-mux--tpgid (pid)
  "Foreground process group of the terminal that controls PID, or nil."
  (when-let* ((stat (ignore-errors
                      (with-temp-buffer
                        (insert-file-contents (format "/proc/%d/stat" pid))
                        (buffer-string))))
              (close (cl-position ?\) stat :from-end t))
              (fields (split-string (substring stat (+ close 2))))
              (tpgid (string-to-number (or (nth 5 fields) "0"))))
    (and (> tpgid 0) tpgid)))

;;;###autoload
(define-minor-mode ghostel-mux-mode
  "Run local ghostel terminals in sessions that outlive Emacs."
  :global t
  (if ghostel-mux-mode
      (progn
        (advice-add 'ghostel--spawn-process :around #'ghostel-mux--spawn-process)
        (advice-add 'ghostel--new :around #'ghostel-mux--new)
        (advice-add 'ghostel--set-size :after #'ghostel-mux--after-set-size)
        (advice-add 'ghostel-foreground-pid :around #'ghostel-mux--foreground-pid)
        (when ghostel-mux-adopt-on-startup
          (ghostel-mux-adopt-detached)))
    (advice-remove 'ghostel--spawn-process #'ghostel-mux--spawn-process)
    (advice-remove 'ghostel--new #'ghostel-mux--new)
    (advice-remove 'ghostel--set-size #'ghostel-mux--after-set-size)
    (advice-remove 'ghostel-foreground-pid #'ghostel-mux--foreground-pid)))

;;; Adopting, detaching and managing sessions

(defun ghostel-mux--buffer-name (info)
  "A buffer name for the session described by INFO."
  (or (alist-get 'name info)
      (let ((title (alist-get 'title info)))
        (if (string-empty-p title) ghostel-buffer-name (format "*ghostel: %s*" title)))))

(defvar ghostel-mux--batch nil
  "Non-nil while a command adopts several sessions in one go.")

(defvar ghostel-mux--skew-answer nil
  "The answer that holds for the rest of a batch adoption, if any.
`accept' adopts every session whose snapshot cannot be restored, and
`skip' leaves every such session as it is.")

(defun ghostel-mux--peek-problem (id)
  "Return why the snapshot of session ID cannot be restored, or nil.
Also return the session's metadata: the value is (PROBLEM . INFO).
Looking does not change who owns the session."
  (let ((ctl (ghostel-mux--connect id))
        (tries 0)
        result)
    (unwind-protect
        (while (not result)
          (ghostel-mux--send ctl '((op . "snapshot")))
          (condition-case err
              (let* ((snapshot (alist-get 'bytes (ghostel-mux--await ctl "snapshot")))
                     (info (alist-get 'info (ghostel-mux--await ctl "info"))))
                (setq result (cons (ghostel-mux--snapshot-error snapshot) info)))
            (error
             ;; The session answers an error while its terminal is in the
             ;; middle of an escape sequence; output soon moves it on.
             (when (> (cl-incf tries) 20)
               (signal (car err) (cdr err)))
             (sleep-for 0.05))))
      (delete-process ctl))
    result))

(defun ghostel-mux--confirm-skew (id info problem)
  "Ask whether to adopt session ID although its snapshot cannot be restored.
INFO is the session's metadata and PROBLEM why the snapshot fails.  In a
batch adoption the user can answer for every remaining session at once."
  (let ((question
         (format "Session %s (%s) cannot be restored here: it was started by a \
ghostel-mux built with a different terminal library (%s).  Adopting it shows only \
new output, and its screen and scrollback are lost.  "
                 id (ghostel-mux--describe-command info) problem)))
    (pcase ghostel-mux--skew-answer
      ('accept t)
      ('skip nil)
      (_
       (if (not ghostel-mux--batch)
           (yes-or-no-p (concat question "Adopt it anyway? "))
         (pcase (car (read-multiple-choice
                      (concat question "Adopt it anyway?")
                      '((?y "yes" "Adopt this session without its history")
                        (?n "no" "Leave this session as it is")
                        (?! "all" "Adopt this one and every other such session in this operation")
                        (?q "none" "Leave this one and every other such session as they are"))))
           (?y t)
           (?! (setq ghostel-mux--skew-answer 'accept) t)
           (?q (setq ghostel-mux--skew-answer 'skip) nil)
           (_ nil)))))))

;;;###autoload
(defun ghostel-mux-adopt (id &optional steal)
  "Show session ID in a new ghostel buffer and return the buffer.
With STEAL non-nil, take the session from the Emacs that owns it;
otherwise an owned session signals an error.  Interactively, prompt
for a session, and a prefix argument means STEAL.

When the session's screen and scrollback cannot be restored, ask first;
declining leaves the session as it was, owner included."
  (interactive (list (ghostel-mux--read-session) current-prefix-arg))
  (ghostel--load-module t)
  (pcase-let ((`(,problem . ,peeked) (ghostel-mux--peek-problem id)))
    (when (and problem (not (ghostel-mux--confirm-skew id peeked problem)))
      (user-error "Left session %s as it was" id)))
  (let ((ctl (ghostel-mux--connect id)))
    (condition-case err
        (progn
          (ghostel-mux--send ctl `((op . "attach")
                                   (client . ,(ghostel-mux--client-name))
                                   (steal . ,(if steal t :false))))
          (let ((reply (ghostel-mux--await ctl "snapshot" "busy")))
            (when (equal (alist-get 'type reply) "busy")
              (user-error "Session %s belongs to %s; adopt it with a prefix argument to take it"
                          id (alist-get 'client (alist-get 'owner reply))))
            (let* ((info (alist-get 'info (ghostel-mux--await ctl "attached")))
                   (snapshot (alist-get 'bytes reply)))
              ;; The user has agreed to lose the history when it cannot be
              ;; restored, so such a snapshot only means a blank terminal.
              (when (ghostel-mux--snapshot-error snapshot)
                (setq snapshot nil))
              (let ((buffer (let ((ghostel-mux--pending-snapshot snapshot))
                              (ghostel--create (generate-new-buffer-name
                                                (ghostel-mux--buffer-name info))
                                               (append display-buffer--same-window-action
                                                       '((category . comint)))))))
                (with-current-buffer buffer
                  (let* ((context `((kind . term) (name . ,ghostel-buffer-name)))
                         (instance (ghostel--next-instance context)))
                    (setq ghostel--managed-buffer-name (buffer-name)
                          ghostel--initial-name (buffer-name)
                          ghostel-identity `(,@context (instance . ,instance))))
                  (ghostel-mux--attach-buffer id ctl info)
                  (ghostel--set-title (alist-get 'title info))
                  (unless (string-empty-p (alist-get 'pwd info))
                    (ghostel--update-directory (alist-get 'pwd info)))
                  (ghostel--invalidate))
                buffer))))
      ((error quit)
       (when (process-live-p ctl) (delete-process ctl))
       (signal (car err) (cdr err))))))

(defun ghostel-mux--adopt-each (pred steal)
  "Adopt every session whose command still runs and whose metadata PRED accepts.
STEAL is passed to `ghostel-mux-adopt'.  The skew prompt offers to
answer for every remaining session.  Returns the adopted buffers."
  (let ((ghostel-mux--batch t)
        (ghostel-mux--skew-answer nil)
        buffers)
    (dolist (info (ghostel-mux--sessions))
      (when (and (not (alist-get 'exit_status info)) (funcall pred info))
        (save-window-excursion
          ;; A session the user declines to adopt stays as it was; go on
          ;; with the rest.
          (condition-case err
              (push (ghostel-mux-adopt (alist-get 'id info) steal) buffers)
            (user-error (message "%s" (error-message-string err)))))))
    (message "Adopted %d terminal%s" (length buffers) (if (= (length buffers) 1) "" "s"))
    (nreverse buffers)))

;;;###autoload
(defun ghostel-mux-adopt-detached ()
  "Adopt every orphaned terminal: each detached session whose command runs.
Returns the adopted buffers."
  (interactive)
  (ghostel-mux--adopt-each (lambda (info) (null (alist-get 'owner info))) nil))

;;;###autoload
(defun ghostel-mux-adopt-all ()
  "Adopt every terminal whose command runs, taking those other Emacsen own.
The Emacs that owned a taken session keeps its buffer, which says the
session was taken.  Returns the adopted buffers."
  (interactive)
  (ghostel-mux--adopt-each
   (lambda (info)
     (let ((owner (alist-get 'owner info)))
       (not (and owner (equal (alist-get 'pid owner) (emacs-pid))))))
   t))

(defun ghostel-mux-detach ()
  "Close the current ghostel buffer and leave its session running."
  (interactive)
  (unless ghostel-mux--id
    (user-error "This buffer is not showing a ghostel-mux session"))
  (when (process-live-p ghostel-mux--ctl)
    (ghostel-mux--send ghostel-mux--ctl '((op . "detach"))))
  (setq ghostel-mux--detaching t)
  (let ((kill-buffer-query-functions nil))
    (kill-buffer (current-buffer))))

(defun ghostel-mux--describe-command (info)
  "The title of the session INFO, or its command when it has no title."
  (if (string-empty-p (alist-get 'title info))
      (string-join (alist-get 'argv info) " ")
    (alist-get 'title info)))

(defun ghostel-mux--describe (info)
  "One line describing the session INFO, for completion."
  (format "%s  %s  %s"
          (alist-get 'id info)
          (ghostel-mux--state info)
          (ghostel-mux--describe-command info)))

(defun ghostel-mux--state (info)
  "Short state of the session INFO."
  (cond ((alist-get 'exit_status info) (format "exited %d" (alist-get 'exit_status info)))
        ((alist-get 'owner info) (format "owned by %s" (alist-get 'client (alist-get 'owner info))))
        (t "detached")))

(defun ghostel-mux--read-session ()
  "Prompt for a session and return its id."
  (let* ((choices (mapcar (lambda (info) (cons (ghostel-mux--describe info) (alist-get 'id info)))
                          (ghostel-mux--sessions))))
    (unless choices (user-error "No ghostel-mux sessions"))
    (cdr (assoc (completing-read "Session: " choices nil t) choices))))

(defvar-keymap ghostel-mux-list-mode-map
  :parent tabulated-list-mode-map
  "RET" #'ghostel-mux-list-adopt
  "s" #'ghostel-mux-list-steal
  "e" #'ghostel-mux-list-evict
  "k" #'ghostel-mux-list-kill
  "o" #'ghostel-mux-list-adopt-detached
  "A" #'ghostel-mux-list-adopt-all)

(define-derived-mode ghostel-mux-list-mode tabulated-list-mode "Ghostel-Mux"
  "Major mode listing ghostel-mux sessions.
`ghostel-mux-evil' gives it Evil bindings.

\\{ghostel-mux-list-mode-map}"
  (setq tabulated-list-format [("Id" 17 t) ("State" 36 t) ("Title" 30 t) ("Directory" 0 t)])
  (setq tabulated-list-padding 1)
  (add-hook 'tabulated-list-revert-hook #'ghostel-mux--list-refresh nil t)
  (tabulated-list-init-header))

(defun ghostel-mux--list-refresh ()
  "Fill `tabulated-list-entries' with the current sessions."
  (setq tabulated-list-entries
        (mapcar (lambda (info)
                  (list (alist-get 'id info)
                        (vector (alist-get 'id info)
                                (ghostel-mux--state info)
                                (alist-get 'title info)
                                (let ((pwd (alist-get 'pwd info)))
                                  (if (string-empty-p pwd) (alist-get 'cwd info) pwd)))))
                (ghostel-mux--sessions))))

;;;###autoload
(defun ghostel-mux-list ()
  "List the ghostel-mux sessions of every Emacs this user runs."
  (interactive)
  (with-current-buffer (get-buffer-create "*ghostel-mux*")
    (ghostel-mux-list-mode)
    (ghostel-mux--list-refresh)
    (tabulated-list-print)
    (pop-to-buffer (current-buffer))))

(defun ghostel-mux--list-id ()
  "Id of the session on the current line."
  (or (tabulated-list-get-id) (user-error "No session on this line")))

(defun ghostel-mux-list-adopt ()
  "Adopt the session on the current line."
  (interactive)
  (ghostel-mux-adopt (ghostel-mux--list-id)))

(defun ghostel-mux-list-steal ()
  "Take the session on the current line from the Emacs that owns it."
  (interactive)
  (ghostel-mux-adopt (ghostel-mux--list-id) t))

(defun ghostel-mux-list-adopt-detached ()
  "Adopt every orphaned terminal, then refresh the list."
  (interactive)
  (ghostel-mux-adopt-detached)
  (revert-buffer))

(defun ghostel-mux-list-adopt-all ()
  "Adopt every terminal, taking those other Emacsen own, then refresh."
  (interactive)
  (when (yes-or-no-p "Take every terminal, including those other Emacsen show? ")
    (ghostel-mux-adopt-all)
    (revert-buffer)))

(defun ghostel-mux-list-evict ()
  "Force the owner of the session on the current line to let go of it."
  (interactive)
  (ghostel-mux--run "evict" (ghostel-mux--list-id))
  (revert-buffer))

(defun ghostel-mux-list-kill ()
  "Hang up the terminal of the session on the current line."
  (interactive)
  (when (yes-or-no-p "Kill this session? ")
    (ghostel-mux--run "kill" (ghostel-mux--list-id))
    (revert-buffer)))

(provide 'ghostel-mux)
;;; ghostel-mux.el ends here
