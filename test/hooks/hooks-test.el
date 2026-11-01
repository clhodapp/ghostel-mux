;;; hooks-test.el --- Batch tests for the ghostel snapshot and socket hooks -*- lexical-binding: t; -*-

(require 'ert)
(require 'ghostel)
(ghostel--load-module)

(defvar hooks-test-dir (file-name-directory (or load-file-name buffer-file-name)))

(defun hooks-test--wait-for (pred &optional process seconds)
  "Run the event loop until PRED returns non-nil or SECONDS pass.
PROCESS, when live, is the process whose output to wait for."
  (let ((deadline (+ (float-time) (or seconds 5))))
    (while (and (not (funcall pred)) (< (float-time) deadline))
      (accept-process-output (and process (process-live-p process) process) 0.05))
    (funcall pred)))

(defun hooks-test--text (buffer)
  "Return the terminal text of ghostel BUFFER."
  (with-current-buffer buffer
    (or (ghostel--copy-all-text ghostel--term) "")))

(ert-deftest hooks-new-from-snapshot ()
  "A snapshot restores the screen and the scrollback, and renders."
  (let ((snap (with-temp-buffer
                (set-buffer-multibyte nil)
                (insert-file-contents-literally
                 (expand-file-name "fixture.snap" hooks-test-dir))
                (buffer-string))))
    (with-temp-buffer
      (ghostel-mode)
      (setq ghostel--term (ghostel--new-from-snapshot snap))
      (let ((inhibit-read-only t))
        (ghostel--redraw ghostel--term t t))
      (let ((text (buffer-substring-no-properties (point-min) (point-max))))
        (should (string-match-p "red line" text))
        (should (string-match-p "history 000" text))
        (should (string-match-p "history 099" text))
        (should (string-match-p "prompt\\$ " text))))))

(ert-deftest hooks-connect-native-socket ()
  "The native reader reads from and writes to a Unix socket."
  (let* ((sock (make-temp-name (expand-file-name "ghostel-hook-" temporary-file-directory)))
         (server (make-process :name "server" :buffer (generate-new-buffer " *server*")
                               :command (list "python3" (expand-file-name "server.py" hooks-test-dir) sock)
                               :connection-type 'pipe))
         (buf (ghostel--create " *hooks-test*" nil 24 80)))
    (unwind-protect
        (with-current-buffer buf
          (should (hooks-test--wait-for
                   (lambda () (with-current-buffer (process-buffer server)
                                (string-match-p "ready" (buffer-string))))
                   server))
          (let ((pipe (make-pipe-process :name "ghostel-native-socket"
                                         :buffer buf
                                         :filter #'ghostel--events-filter
                                         :noquery t)))
            (setq ghostel--process pipe)
            (should (eq t (ghostel--connect-native-socket ghostel--term sock pipe)))
            (should (hooks-test--wait-for
                     (lambda () (string-match-p "hello from socket" (hooks-test--text buf)))
                     pipe))
            (ghostel--write-pty ghostel--term "abc")
            (should (hooks-test--wait-for
                     (lambda () (string-match-p "got:abc" (hooks-test--text buf)))
                     pipe))
            (ghostel--write-pty ghostel--term "bye")
            ;; The peer closing the socket ends the reader; the pipe then
            ;; receives exit status 0 and closes.
            (should (hooks-test--wait-for (lambda () (not (process-live-p pipe))) pipe))
            (should (eql 0 (process-get pipe 'ghostel--exit-status)))))
      (delete-process server)
      (when (buffer-live-p buf)
        (let ((kill-buffer-query-functions nil))
          (kill-buffer buf))))))

;;; hooks-test.el ends here
