;;; snake.el --- 简单贪吃蛇 (Emacs Lisp)  -*- lexical-binding: t; -*-

;; 用法: M-x load-file RET snake.el RET 然后 M-x snake RET
;; 操作: 方向键 / h j k l 移动, q 退出, 空格 暂停

;;; Code:

(require 'cl-lib)

(defvar snake--width 40)
(defvar snake--height 20)
(defvar snake--body nil "蛇身坐标列表，car 为蛇头。")
(defvar snake--dir '(1 . 0))
(defvar snake--pending nil "缓存的下一步方向，避免一帧内连续转向自杀。")
(defvar snake--food nil)
(defvar snake--score 0)
(defvar snake--timer nil)
(defvar snake--game-over nil)
(defvar snake--paused nil)
(defvar snake--buffer "*snake*")
(defvar snake--speed 0.15 "每步间隔秒数。")

(define-derived-mode snake-mode special-mode "Snake"
  "贪吃蛇小游戏的 major mode。"
  (setq-local cursor-type nil)
  (setq-local truncate-lines t)
  (buffer-disable-undo))

(defun snake--random-empty-cell ()
  "在棋盘上随机找一个没被蛇占据的格子。"
  (let (cell)
    (while (not cell)
      (let ((c (cons (random snake--width) (random snake--height))))
        (unless (snake--occupied-p c)
          (setq cell c))))
    cell))

(defun snake--occupied-p (cell)
  (cl-some (lambda (seg) (equal seg cell)) snake--body))

(defun snake--reset ()
  (setq snake--width 40
        snake--height 20
        snake--score 0
        snake--dir '(1 . 0)
        snake--pending nil
        snake--game-over nil
        snake--paused nil)
  (let ((cx (/ snake--width 2)) (cy (/ snake--height 2)))
    (setq snake--body (list (cons cx cy)
                            (cons (1- cx) cy)
                            (cons (- cx 2) cy))))
  (setq snake--food (snake--random-empty-cell)))

(defun snake--draw ()
  (let ((inhibit-read-only t)
        (lines nil))
    ;; 从上往下逐行构建
    (cl-loop for y downfrom (1- snake--height) to 0 do
             (let ((row nil))
               (cl-loop for x from 0 to (1- snake--width) do
                        (let ((cell (cons x y)))
                          (push (cond
                                 ((equal cell (car snake--body)) ?@)
                                 ((snake--occupied-p cell) ?o)
                                 ((equal cell snake--food) ?*)
                                 (t ?. ))
                                row)))
               (push (apply #'string (nreverse row)) lines)))
    (erase-buffer)
    (insert (format " 分数: %d   长度: %d   %s\n\n"
                    snake--score
                    (length snake--body)
                    (cond (snake--game-over "游戏结束! 按 r 重开, q 退出")
                          (snake--paused "已暂停, 按空格继续")
                          (t "方向键移动, 空格暂停, q 退出"))))
    (insert (mapconcat #'identity lines "\n"))
    (goto-char (point-min))))

(defun snake--step ()
  (when (and (not snake--paused) (not snake--game-over))
    (when snake--pending
      (setq snake--dir snake--pending
            snake--pending nil))
    (let* ((head (car snake--body))
           (nx (+ (car head) (car snake--dir)))
           (ny (+ (cdr head) (cdr snake--dir)))
           (new-head (cons nx ny))
           (hit-wall (or (< nx 0) (>= nx snake--width)
                         (< ny 0) (>= ny snake--height)))
           ;; 尾巴这一帧会移走，所以允许走进当前尾巴格
           (hit-self (and (not hit-wall)
                          (let ((tail (car (last snake--body))))
                            (and (not (equal new-head tail))
                                 (snake--occupied-p new-head))))))
      (cond
       ((or hit-wall hit-self)
        (setq snake--game-over t)
        (snake--draw))
       (t
        (push new-head snake--body)
        (if (equal new-head snake--food)
            (progn
              (setq snake--score (+ snake--score 10))
              (setq snake--food (snake--random-empty-cell)))
          ;; 没吃到就扔掉尾巴
          (setq snake--body (nreverse (cdr (nreverse snake--body)))))
        (snake--draw))))))

(defun snake--set-dir (dx dy)
  "设置方向，禁止 180 度掉头。"
  (unless (and (= dx (- (car snake--dir))) (= dy (- (cdr snake--dir))))
    (setq snake--pending (cons dx dy))))

(defun snake-up ()    (interactive) (snake--set-dir 0 1))
(defun snake-down ()  (interactive) (snake--set-dir 0 -1))
(defun snake-left ()  (interactive) (snake--set-dir -1 0))
(defun snake-right () (interactive) (snake--set-dir 1 0))

(defun snake-toggle-pause ()
  (interactive)
  (unless snake--game-over
    (setq snake--paused (not snake--paused))
    (snake--draw)))

(defun snake-quit ()
  (interactive)
  (when (timerp snake--timer)
    (cancel-timer snake--timer))
  (setq snake--timer nil)
  (quit-window t))

(defun snake-restart ()
  (interactive)
  (snake--reset)
  (snake--draw))

(defvar snake-mode-map
  (let ((map (make-sparse-keymap)))
    (set-keymap-parent map special-mode-map)
    (define-key map (kbd "<up>")    #'snake-up)
    (define-key map (kbd "<down>")  #'snake-down)
    (define-key map (kbd "<left>")  #'snake-left)
    (define-key map (kbd "<right>") #'snake-right)
    (define-key map "k" #'snake-up)
    (define-key map "j" #'snake-down)
    (define-key map "h" #'snake-left)
    (define-key map "l" #'snake-right)
    (define-key map " " #'snake-toggle-pause)
    (define-key map "r" #'snake-restart)
    (define-key map "q" #'snake-quit)
    map))

;;;###autoload
(defun snake ()
  "启动贪吃蛇。"
  (interactive)
  (let ((buf (get-buffer-create snake--buffer)))
    (with-current-buffer buf
      (snake-mode)
      (snake--reset)
      (snake--draw))
    (switch-to-buffer buf)
    (when (timerp snake--timer)
      (cancel-timer snake--timer))
    (setq snake--timer
          (run-with-timer snake--speed snake--speed
                          (lambda ()
                            (when (buffer-live-p buf)
                              (with-current-buffer buf
                                (if snake--game-over
                                    (when (timerp snake--timer)
                                      (cancel-timer snake--timer)
                                      (setq snake--timer nil))
                                  (snake--step)))))))))

(provide 'snake)
;;; snake.el ends here
