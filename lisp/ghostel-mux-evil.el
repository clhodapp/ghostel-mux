;;; ghostel-mux-evil.el --- Evil bindings for ghostel-mux -*- lexical-binding: t; -*-

;; Copyright (C) 2026 Chris Hodapp
;; SPDX-License-Identifier: GPL-3.0-or-later
;; Package-Requires: ((emacs "30.1") (evil "1.15") (ghostel-mux "0.1.0"))

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

;; Loading this file makes `ghostel-mux-list' start in Evil's normal state
;; and binds the list's commands there and in motion state.  `j' and `k'
;; keep moving between rows; the keys below replace Vim's text-editing
;; meanings, which a session list has no use for.
;;
;;   RET  adopt the session       s    take it from its owner
;;   E    evict the owner         D    kill the session
;;   o    adopt the orphans       A    adopt every terminal
;;   g r  refresh                 q    close the list
;;
;; Load it after Evil and ghostel-mux, for example with
;; (with-eval-after-load 'evil (require 'ghostel-mux-evil)).

;;; Code:

(require 'evil)
(require 'ghostel-mux)

(evil-set-initial-state 'ghostel-mux-list-mode 'normal)

(evil-define-key* '(normal motion) ghostel-mux-list-mode-map
  (kbd "RET") #'ghostel-mux-list-adopt
  "s" #'ghostel-mux-list-steal
  "E" #'ghostel-mux-list-evict
  "D" #'ghostel-mux-list-kill
  "o" #'ghostel-mux-list-adopt-detached
  "A" #'ghostel-mux-list-adopt-all
  (kbd "g r") #'revert-buffer
  "q" #'quit-window)

(provide 'ghostel-mux-evil)
;;; ghostel-mux-evil.el ends here
