;;; throughput-bench.el --- Output throughput, session against native PTY -*- lexical-binding: t; -*-

;; Times how long `seq 1 N' takes to reach a ghostel terminal, once with
;; ghostel's own native PTY and once through a ghostel-mux session.  Prints
;; one line per run.

(require 'ghostel-mux)

(setq ghostel-mux-program (getenv "GHOSTEL_MUX")
      ghostel-shell "/bin/sh"
      ghostel-shell-integration nil
      ghostel-query-before-killing nil)
(setenv "GHOSTEL_MUX_LAUNCHER" "fork")

(defconst throughput-bench-lines 2000000)

(defun throughput-bench--run (label)
  "Time one run in a fresh terminal, reporting it under LABEL."
  (let* ((buffer (ghostel-create (format "bench-%s" label)))
         (last (format "\n%d" throughput-bench-lines))
         (done (lambda ()
                 (with-current-buffer buffer
                   (string-search last (or (ghostel--copy-all-text ghostel--term) "")))))
         (start (float-time)))
    (with-current-buffer buffer
      (ghostel--send-string (format "seq 1 %d; echo bench-done\n" throughput-bench-lines)))
    (while (not (funcall done))
      (accept-process-output nil 0.01))
    (let ((elapsed (- (float-time) start)))
      (message "%-8s %.3f s  %.1f MB/s" label elapsed
               (/ (/ (* 7.0 throughput-bench-lines) 1e6) elapsed)))
    (let ((kill-buffer-query-functions nil))
      (kill-buffer buffer))))

(dotimes (_ 3)
  (ghostel-mux-mode -1)
  (throughput-bench--run "native")
  (ghostel-mux-mode 1)
  (throughput-bench--run "session"))

;;; throughput-bench.el ends here
