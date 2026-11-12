;;; ghostel-mux-test.el --- End-to-end tests for ghostel-mux -*- lexical-binding: t; -*-

;; Run in batch with the patched ghostel on the load path, GHOSTEL_MUX set
;; to the ghostel-mux binary, and a private XDG_RUNTIME_DIR.

(require 'ert)
(require 'ghostel-mux)

(setq ghostel-mux-program (getenv "GHOSTEL_MUX")
      ghostel-shell "/bin/sh"
      ghostel-shell-integration nil
      ghostel-query-before-killing nil)
(setenv "GHOSTEL_MUX_LAUNCHER" "fork")
(setenv "PS1" "$ ")
(ghostel-mux-mode 1)

(defun ghostel-mux-test--wait (pred &optional seconds)
  "Run the event loop until PRED returns non-nil, or fail after SECONDS."
  (let ((deadline (+ (float-time) (or seconds 5))))
    (while (and (not (funcall pred)) (< (float-time) deadline))
      (accept-process-output nil 0.05))
    (or (funcall pred) (ert-fail "timed out"))))

(defun ghostel-mux-test--text (buffer)
  "All terminal text of ghostel BUFFER."
  (with-current-buffer buffer
    (or (and ghostel--term (ghostel--copy-all-text ghostel--term)) "")))

(defun ghostel-mux-test--wait-text (buffer regexp)
  "Wait until the terminal text of BUFFER matches REGEXP."
  (ghostel-mux-test--wait (lambda () (string-match-p regexp (ghostel-mux-test--text buffer)))))

(defun ghostel-mux-test--session (id)
  "The listed metadata of session ID, or nil."
  (cl-find id (ghostel-mux--sessions) :key (lambda (i) (alist-get 'id i)) :test #'equal))

(defun ghostel-mux-test--type (buffer string)
  "Type STRING into the terminal of BUFFER."
  (with-current-buffer buffer
    (ghostel--send-string string)))

(ert-deftest ghostel-mux-start-detach-adopt ()
  "A terminal survives its buffer and comes back with its output."
  (let* ((buffer (ghostel-create "mux-test"))
         (id (buffer-local-value 'ghostel-mux--id buffer)))
    (should id)
    (ghostel-mux-test--type buffer "echo hello-$((40+2))\n")
    (ghostel-mux-test--wait-text buffer "hello-42")
    (should (string-match-p "emacs" (alist-get 'client (alist-get 'owner (ghostel-mux-test--session id)))))

    (with-current-buffer buffer (ghostel-mux-detach))
    (should-not (buffer-live-p buffer))
    (ghostel-mux-test--wait (lambda () (null (alist-get 'owner (ghostel-mux-test--session id)))))

    (let ((adopted (ghostel-mux-adopt id)))
      ;; The earlier output is there before the shell prints anything new.
      (should (string-match-p "hello-42" (ghostel-mux-test--text adopted)))
      (ghostel-mux-test--type adopted "echo again-$((1+1))\n")
      (ghostel-mux-test--wait-text adopted "again-2")
      (kill-buffer adopted))
    ;; Killing the buffer hangs up the terminal and ends the session.
    (ghostel-mux-test--wait (lambda () (null (ghostel-mux-test--session id))) 8)))

(ert-deftest ghostel-mux-exit-status ()
  "The command's exit status reaches `ghostel-exit-functions'."
  (let* ((events nil)
         (ghostel-exit-functions (list (lambda (_buffer event) (push event events))))
         (buffer (ghostel-create "mux-exit")))
    (ghostel-mux-test--type buffer "exit 7\n")
    (ghostel-mux-test--wait (lambda () events))
    (should (equal events '("exited abnormally with code 7\n")))))

(ert-deftest ghostel-mux-evicted-buffer-keeps-its-text ()
  "A buffer whose session another client takes says so and stays."
  (let* ((buffer (ghostel-create "mux-evict"))
         (id (buffer-local-value 'ghostel-mux--id buffer)))
    (ghostel-mux-test--type buffer "echo before-$((2+3))\n")
    (ghostel-mux-test--wait-text buffer "before-5")
    (ghostel-mux--run "evict" id)
    (ghostel-mux-test--wait-text buffer "detached by another client")
    (should (buffer-live-p buffer))
    (should (string-match-p "before-5" (ghostel-mux-test--text buffer)))
    (should (alist-get 'id (ghostel-mux-test--session id)))
    (kill-buffer buffer)
    (ghostel-mux--run "kill" id)))

(ert-deftest ghostel-mux-steal-from-another-owner ()
  "Adopting an owned session needs STEAL, which takes it."
  (let* ((buffer (ghostel-create "mux-steal"))
         (id (buffer-local-value 'ghostel-mux--id buffer)))
    (should-error (ghostel-mux-adopt id) :type 'user-error)
    (let ((thief (ghostel-mux-adopt id t)))
      (ghostel-mux-test--wait-text buffer "taken by")
      (ghostel-mux-test--type thief "echo stolen-$((3+3))\n")
      (ghostel-mux-test--wait-text thief "stolen-6")
      (kill-buffer thief))
    (kill-buffer buffer)
    (ghostel-mux-test--wait (lambda () (null (ghostel-mux-test--session id))) 8)))

(defun ghostel-mux-test--snapshot-images (snapshot)
  "Build a terminal from SNAPSHOT and return how many images ghostel draws.
The terminal lives in a temporary buffer, as in the ghostel Kitty
tests: ghostel emits image placements only on a graphical display, so
the check is stubbed, and its renderer cannot measure glyphs for a
displayed buffer in a batch Emacs."
  (let ((count 0))
    (with-temp-buffer
      (let ((term (ghostel--new-from-snapshot snapshot))
            (inhibit-read-only t))
        (cl-letf (((symbol-function 'ghostel--kitty-display-image)
                   (lambda (&rest _) (cl-incf count)))
                  ((symbol-function 'display-graphic-p) (lambda (&rest _) t)))
          (ghostel--redraw term t))))
    count))

(defun ghostel-mux-test--take-snapshot (id)
  "Attach to session ID, return its snapshot, and detach again."
  (let ((ctl (ghostel-mux--connect id)))
    (unwind-protect
        (progn
          (ghostel-mux--send ctl `((op . "attach") (client . "test")))
          (prog1 (alist-get 'bytes (ghostel-mux--await ctl "snapshot"))
            (ghostel-mux--await ctl "attached")))
      (delete-process ctl))))

(ert-deftest ghostel-mux-kitty-image-survives-adopt ()
  "A Kitty graphics image on screen at a detach is in the adopt snapshot."
  (let* ((buffer (ghostel-create "mux-kitty"))
         (id (buffer-local-value 'ghostel-mux--id buffer)))
    ;; Transmit and place a 2x2 white RGB image, then print a marker.
    (ghostel-mux-test--type
     buffer "printf '\\033_Ga=T,f=24,s=2,v=2,c=4,r=2;////////////////\\033\\\\'; echo img-$((3*3))\n")
    (ghostel-mux-test--wait-text buffer "img-9")
    (with-current-buffer buffer (ghostel-mux-detach))
    (ghostel-mux-test--wait (lambda () (null (alist-get 'owner (ghostel-mux-test--session id)))))
    (should (> (ghostel-mux-test--snapshot-images (ghostel-mux-test--take-snapshot id)) 0))
    (ghostel-mux--run "kill" id)))

(defun ghostel-mux-test--detached-session ()
  "Start a terminal, print a marker, detach it, and return its id."
  (let* ((buffer (ghostel-create "mux-old"))
         (id (buffer-local-value 'ghostel-mux--id buffer)))
    (ghostel-mux-test--type buffer "echo old-$((4+4))\n")
    (ghostel-mux-test--wait-text buffer "old-8")
    (with-current-buffer buffer (ghostel-mux-detach))
    (ghostel-mux-test--wait (lambda () (null (alist-get 'owner (ghostel-mux-test--session id)))))
    id))

(defmacro ghostel-mux-test--with-unreadable-snapshots (answer prompts &rest body)
  "Run BODY as if no snapshot could be restored, answering prompts with ANSWER.
Each prompt is pushed onto the variable named PROMPTS."
  (declare (indent 2))
  `(cl-letf (((symbol-function 'ghostel--new-from-snapshot)
              (lambda (&rest _) (error "Unsupported snapshot version")))
             ((symbol-function 'yes-or-no-p)
              (lambda (prompt) (push prompt ,prompts) ,answer)))
     ,@body))

(ert-deftest ghostel-mux-unreadable-snapshot-declined ()
  "Declining to adopt an unrestorable session leaves it detached."
  (let ((id (ghostel-mux-test--detached-session))
        (prompts nil))
    (ghostel-mux-test--with-unreadable-snapshots nil prompts
      (should-error (ghostel-mux-adopt id) :type 'user-error))
    (should (= (length prompts) 1))
    (should (string-match-p "screen and scrollback are lost" (car prompts)))
    (should-not (cl-find-if (lambda (b) (equal (buffer-local-value 'ghostel-mux--id b) id))
                            (buffer-list)))
    (ghostel-mux-test--wait (lambda () (null (alist-get 'owner (ghostel-mux-test--session id)))))
    (ghostel-mux--run "kill" id)))

(ert-deftest ghostel-mux-unreadable-snapshot-accepted ()
  "Accepting adopts the session with a blank terminal that works."
  (let ((id (ghostel-mux-test--detached-session))
        (prompts nil)
        adopted)
    (ghostel-mux-test--with-unreadable-snapshots t prompts
      (setq adopted (ghostel-mux-adopt id)))
    (should (= (length prompts) 1))
    (should-not (string-match-p "old-8" (ghostel-mux-test--text adopted)))
    (ghostel-mux-test--type adopted "echo new-$((5+5))\n")
    (ghostel-mux-test--wait-text adopted "new-10")
    (kill-buffer adopted)
    (ghostel-mux-test--wait (lambda () (null (ghostel-mux-test--session id))) 8)))

(defun ghostel-mux-test--buffer-of (id)
  "The live buffer of this Emacs showing session ID, or nil."
  (cl-find-if (lambda (b) (equal (buffer-local-value 'ghostel-mux--id b) id))
              (buffer-list)))

(defun ghostel-mux-test--cleanup (ids)
  "Kill the buffers and sessions of IDS."
  (dolist (id ids)
    (when-let* ((buffer (ghostel-mux-test--buffer-of id)))
      (kill-buffer buffer))
    ;; Killing the buffer already ends its session, which may exit between
    ;; the listing and the kill; a kill that finds no session is fine here.
    (when (ghostel-mux-test--session id)
      (ignore-errors (ghostel-mux--run "kill" id)))))

(ert-deftest ghostel-mux-adopt-detached-adopts-orphans-only ()
  "Orphaned terminals are adopted; terminals this Emacs shows are left alone."
  (let* ((own-id (buffer-local-value 'ghostel-mux--id (ghostel-create "mux-own")))
         (orphans (list (ghostel-mux-test--detached-session)
                        (ghostel-mux-test--detached-session)))
         (adopted (ghostel-mux-adopt-detached)))
    (should (equal (sort (mapcar (lambda (b) (buffer-local-value 'ghostel-mux--id b)) adopted)
                         #'string<)
                   (sort (copy-sequence orphans) #'string<)))
    (dolist (buffer adopted)
      (should (string-match-p "old-8" (ghostel-mux-test--text buffer))))
    (ghostel-mux-test--cleanup (cons own-id orphans))))

(defun ghostel-mux-test--foreign-owner ()
  "Start a child Emacs that owns a new session and stays running.
Returns (PROCESS . ID)."
  (let* ((id-file (make-temp-file "ghostel-mux-foreign-id"))
         (form `(progn
                  (setq ghostel-mux-program ,ghostel-mux-program
                        ghostel-shell "/bin/sh"
                        ghostel-shell-integration nil)
                  (ghostel-mux-mode 1)
                  (let ((buffer (ghostel-create "foreign")))
                    (with-current-buffer buffer
                      (ghostel--send-string "echo foreign-$((7+7))\n"))
                    (while (not (string-match-p
                                 "foreign-14"
                                 (with-current-buffer buffer
                                   (or (ghostel--copy-all-text ghostel--term) ""))))
                      (accept-process-output nil 0.05))
                    (write-region (buffer-local-value 'ghostel-mux--id buffer) nil ,id-file)
                    (while t (accept-process-output nil 1)))))
         (process (make-process
                   :name "foreign-emacs"
                   :command (list (expand-file-name invocation-name invocation-directory)
                                  "--batch"
                                  "-L" (file-name-directory (locate-library "ghostel-mux"))
                                  "-l" "ghostel-mux"
                                  "--eval" (prin1-to-string form))
                   :buffer (generate-new-buffer " *foreign-emacs*")
                   :noquery t)))
    (ghostel-mux-test--wait
     (lambda () (> (file-attribute-size (file-attributes id-file)) 0)) 15)
    (prog1 (cons process (with-temp-buffer
                           (insert-file-contents id-file)
                           (buffer-string)))
      (delete-file id-file))))

(ert-deftest ghostel-mux-adopt-all-takes-from-other-emacsen ()
  "Adopting all terminals takes the ones another running Emacs owns."
  (pcase-let* ((`(,foreign . ,foreign-id) (ghostel-mux-test--foreign-owner))
               (orphan (ghostel-mux-test--detached-session)))
    (unwind-protect
        (let ((adopted (ghostel-mux-adopt-all)))
          (should (= (length adopted) 2))
          (should (= (alist-get 'pid (alist-get 'owner (ghostel-mux-test--session foreign-id)))
                     (emacs-pid)))
          (should (string-match-p "foreign-14"
                                  (ghostel-mux-test--text (ghostel-mux-test--buffer-of foreign-id))))
          (should (ghostel-mux-test--buffer-of orphan)))
      (delete-process foreign)
      (ghostel-mux-test--cleanup (list foreign-id orphan)))))

(defmacro ghostel-mux-test--with-batch-skew (answer prompts &rest body)
  "Run BODY with no snapshot restorable, answering the batch prompt with ANSWER.
Each prompt is pushed onto the variable named PROMPTS."
  (declare (indent 2))
  `(cl-letf (((symbol-function 'ghostel--new-from-snapshot)
              (lambda (&rest _) (error "Unsupported snapshot version")))
             ((symbol-function 'read-multiple-choice)
              (lambda (prompt choices &rest _)
                (push prompt ,prompts)
                (assq ,answer choices))))
     ,@body))

(ert-deftest ghostel-mux-batch-skew-accept-all ()
  "Answering \"all\" once adopts every session with an unreadable snapshot."
  (let ((orphans (list (ghostel-mux-test--detached-session)
                       (ghostel-mux-test--detached-session)))
        (prompts nil)
        adopted)
    (ghostel-mux-test--with-batch-skew ?! prompts
      (setq adopted (ghostel-mux-adopt-detached)))
    (should (= (length prompts) 1))
    (should (string-match-p "screen and scrollback are lost" (car prompts)))
    (should (= (length adopted) 2))
    (ghostel-mux-test--cleanup orphans)))

(ert-deftest ghostel-mux-batch-skew-skip-all ()
  "Answering \"none\" once leaves every session with an unreadable snapshot."
  (let ((orphans (list (ghostel-mux-test--detached-session)
                       (ghostel-mux-test--detached-session)))
        (prompts nil)
        adopted)
    (ghostel-mux-test--with-batch-skew ?q prompts
      (setq adopted (ghostel-mux-adopt-detached)))
    (should (= (length prompts) 1))
    (should (null adopted))
    (dolist (id orphans)
      (should (null (alist-get 'owner (ghostel-mux-test--session id)))))
    (ghostel-mux-test--cleanup orphans)))

(ert-deftest ghostel-mux-evil-is-separate ()
  "Evil and ghostel-mux alone leave the session list with the default Evil keys."
  (let ((output
         (with-temp-buffer
           (call-process (expand-file-name invocation-name invocation-directory) nil t nil
                         "--batch"
                         "-L" (file-name-directory (locate-library "ghostel-mux"))
                         "--eval"
                         (prin1-to-string
                          `(progn
                             (setq ghostel-mux-program ,ghostel-mux-program)
                             (require 'evil)
                             (require 'ghostel-mux)
                             (evil-mode 1)
                             (ghostel-mux-list)
                             (with-current-buffer "*ghostel-mux*"
                               (princ (format "loaded=%s s=%s\n"
                                              (featurep 'ghostel-mux-evil)
                                              (key-binding (kbd "s"))))))))
           (buffer-string))))
    (should (string-match-p "loaded=nil s=evil-substitute" output))))

(ert-deftest ghostel-mux-list-evil-bindings ()
  "With ghostel-mux-evil, the list starts in normal state with its commands bound."
  (require 'ghostel-mux-evil)
  (let ((evil-mode-was evil-mode))
    (unwind-protect
        (progn
          (evil-mode 1)
          (ghostel-mux-list)
          (with-current-buffer "*ghostel-mux*"
            (should (eq evil-state 'normal))
            (pcase-dolist (`(,key . ,command)
                           '(("RET" . ghostel-mux-list-adopt)
                             ("s" . ghostel-mux-list-steal)
                             ("E" . ghostel-mux-list-evict)
                             ("D" . ghostel-mux-list-kill)
                             ("o" . ghostel-mux-list-adopt-detached)
                             ("A" . ghostel-mux-list-adopt-all)
                             ("g r" . revert-buffer)
                             ("q" . quit-window)
                             ("j" . evil-next-line)))
              (should (eq (key-binding (kbd key)) command)))
            (kill-buffer)))
      (unless evil-mode-was (evil-mode -1)))))

(defun ghostel-mux-test--child-emacs (how)
  "Start a terminal in a child Emacs, end that Emacs, and return the session id.
HOW is `exit' to call `kill-emacs' or `crash' for the child to SIGKILL
itself; either way no buffer is killed first."
  (let* ((emacs (expand-file-name invocation-name invocation-directory))
         ;; A file rather than standard output, which SIGKILL would discard
         ;; unflushed.
         (id-file (make-temp-file "ghostel-mux-child-id"))
         (form `(progn
                  (setq ghostel-mux-program ,ghostel-mux-program
                        ghostel-shell "/bin/sh"
                        ghostel-shell-integration nil)
                  (ghostel-mux-mode 1)
                  (let* ((buffer (ghostel-create "child"))
                         (deadline (+ (float-time) 5)))
                    (with-current-buffer buffer
                      (ghostel--send-string "echo from-child-$((5+5))\n"))
                    (while (and (< (float-time) deadline)
                                (not (string-match-p
                                      "from-child-10"
                                      (with-current-buffer buffer
                                        (or (ghostel--copy-all-text ghostel--term) "")))))
                      (accept-process-output nil 0.05))
                    (write-region (format "ID=%s\n" (buffer-local-value 'ghostel-mux--id buffer))
                                  nil ,id-file)
                    ,(if (eq how 'crash)
                         '(signal-process (emacs-pid) 9)
                       '(kill-emacs 0)))))
         (output (with-temp-buffer
                   (call-process emacs nil t nil "--batch"
                                 "-L" (file-name-directory (locate-library "ghostel-mux"))
                                 "-l" "ghostel-mux"
                                 "--eval" (prin1-to-string form))
                   (insert-file-contents id-file)
                   (delete-file id-file)
                   (buffer-string))))
    (unless (string-match "ID=\\([0-9a-f]+\\)" output)
      (ert-fail (format "child Emacs printed no session id: %s" output)))
    (match-string 1 output)))

(defun ghostel-mux-test--adopt-after (how)
  "Adopt the session of a child Emacs that ended by HOW, and check it."
  (let ((id (ghostel-mux-test--child-emacs how)))
    (ghostel-mux-test--wait (lambda () (null (alist-get 'owner (ghostel-mux-test--session id)))))
    (let ((adopted (ghostel-mux-adopt id)))
      (should (string-match-p "from-child-10" (ghostel-mux-test--text adopted)))
      (ghostel-mux-test--type adopted "echo adopted-$((6+6))\n")
      (ghostel-mux-test--wait-text adopted "adopted-12")
      (kill-buffer adopted))
    (ghostel-mux-test--wait (lambda () (null (ghostel-mux-test--session id))) 8)))

(ert-deftest ghostel-mux-survives-emacs-exit ()
  "A terminal outlives the Emacs that started it when that Emacs exits."
  (ghostel-mux-test--adopt-after 'exit))

(ert-deftest ghostel-mux-survives-emacs-crash ()
  "A terminal outlives the Emacs that started it when that Emacs is killed."
  (ghostel-mux-test--adopt-after 'crash))

;;; ghostel-mux-test.el ends here
