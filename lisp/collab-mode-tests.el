;;; collab-mode-tests.el --- Unit tests for collab-mode -*- lexical-binding: t; -*-

;; Copyright (C) 2024 Yuan Fu

;; This file is part of collab-mode

;;; Commentary:

;; Unit tests for collab-mode functions.

;;; Code:

(require 'ert)
(require 'collab-mode)

;;; Tests for collab--file-desc-parent

(ert-deftest collab--file-desc-parent/project-root ()
  "Test that project root returns nil (no parent)."
  (let ((file-desc (collab--make-file-desc "host1" "myproject" "")))
    (should (equal nil (collab--file-desc-parent file-desc)))))

(ert-deftest collab--file-desc-parent/standalone-buffer ()
  "Test that standalone buffer in _buffers returns _buffers root."
  (let ((file-desc (collab--make-file-desc "host1" "_buffers" "buffer123")))
    (should (equal (collab--make-file-desc "host1" "_buffers" "")
                   (collab--file-desc-parent file-desc)))))

(ert-deftest collab--file-desc-parent/file-in-root ()
  "Test that file in project root returns project root."
  (let ((file-desc (collab--make-file-desc "host1" "myproject" "file.txt")))
    (should (equal (collab--make-file-desc "host1" "myproject" "")
                   (collab--file-desc-parent file-desc)))))

(ert-deftest collab--file-desc-parent/file-in-subdirectory ()
  "Test that file in subdirectory returns the subdirectory."
  (let ((file-desc (collab--make-file-desc "host1" "myproject" "src/main.c")))
    (should (equal (collab--make-file-desc "host1" "myproject" "src")
                   (collab--file-desc-parent file-desc)))))

(ert-deftest collab--file-desc-parent/deep-nested-file ()
  "Test that deeply nested file returns correct parent."
  (let ((file-desc (collab--make-file-desc "host1" "myproject" "src/lib/utils/helper.c")))
    (should (equal (collab--make-file-desc "host1" "myproject" "src/lib/utils")
                   (collab--file-desc-parent file-desc)))))

(ert-deftest collab--file-desc-parent/directory-with-trailing-slash ()
  "Test that directory path with trailing slash returns parent correctly."
  (let ((file-desc (collab--make-file-desc "host1" "myproject" "src/lib/")))
    ;; file-name-directory of "src/lib/" returns "src/lib/",
    ;; which after trimming becomes "src/lib"
    (should (equal (collab--make-file-desc "host1" "myproject" "src/lib")
                   (collab--file-desc-parent file-desc)))))

;;; Tests for collab--parse-filename

(ert-deftest collab--parse-filename/basic-file ()
  "Test parsing basic filename."
  (should (equal (collab--make-file-desc "host1" "myproject" "file.txt")
                 (collab--parse-filename "/host1/myproject/file.txt"))))

(ert-deftest collab--parse-filename/project-root ()
  "Test parsing project root path."
  (should (equal (collab--make-file-desc "host1" "myproject" "")
                 (collab--parse-filename "/host1/myproject"))))

(ert-deftest collab--parse-filename/nested-file ()
  "Test parsing nested file path."
  (should (equal (collab--make-file-desc "host1" "myproject" "src/lib/utils.c")
                 (collab--parse-filename "/host1/myproject/src/lib/utils.c"))))

(ert-deftest collab--parse-filename/standalone-buffer ()
  "Test parsing standalone buffer path."
  (should (equal (collab--make-file-desc "host1" "_buffers" "buffer123")
                 (collab--parse-filename "/host1/_buffers/buffer123"))))

(ert-deftest collab--parse-filename/invalid-path ()
  "Test that invalid path returns nil."
  (should (equal nil (collab--parse-filename "/host1")))
  (should (equal nil (collab--parse-filename "/")))
  (should (equal nil (collab--parse-filename ""))))

(ert-deftest collab--parse-filename/with-trailing-slash ()
  "Test parsing path with trailing slash."
  (should (equal (collab--make-file-desc "host1" "myproject" "src")
                 (collab--parse-filename "/host1/myproject/src/"))))

(ert-deftest collab--parse-filename/with-spaces ()
  "Test parsing filename with spaces."
  (should (equal (collab--make-file-desc "host1" "myproject" "my file.txt")
                 (collab--parse-filename "/host1/myproject/my file.txt"))))

(ert-deftest collab--parse-filename/special-characters ()
  "Test parsing filename with special characters."
  (should (equal (collab--make-file-desc "host-id" "my-project" "file-name.test.txt")
                 (collab--parse-filename "/host-id/my-project/file-name.test.txt"))))

;;; Tests for collab--encode-filename

(ert-deftest collab--encode-filename/basic-file ()
  "Test encoding basic file descriptor."
  (let ((file-desc (collab--make-file-desc "host1" "myproject" "file.txt")))
    (should (equal "/host1/myproject/file.txt"
                   (collab--encode-filename file-desc)))))

(ert-deftest collab--encode-filename/project-root ()
  "Test encoding project root file descriptor."
  (let ((file-desc (collab--make-file-desc "host1" "myproject" "")))
    (should (equal "/host1/myproject"
                   (collab--encode-filename file-desc)))))

(ert-deftest collab--encode-filename/nested-file ()
  "Test encoding nested file descriptor."
  (let ((file-desc (collab--make-file-desc "host1" "myproject" "src/lib/utils.c")))
    (should (equal "/host1/myproject/src/lib/utils.c"
                   (collab--encode-filename file-desc)))))

(ert-deftest collab--encode-filename/standalone-buffer ()
  "Test encoding standalone buffer file descriptor."
  (let ((file-desc (collab--make-file-desc "host1" "_buffers" "buffer123")))
    (should (equal "/host1/_buffers/buffer123"
                   (collab--encode-filename file-desc)))))

(ert-deftest collab--encode-filename/with-spaces ()
  "Test encoding file descriptor with spaces."
  (let ((file-desc (collab--make-file-desc "host1" "myproject" "my file.txt")))
    (should (equal "/host1/myproject/my file.txt"
                   (collab--encode-filename file-desc)))))

(ert-deftest collab--encode-filename/special-characters ()
  "Test encoding file descriptor with special characters."
  (let ((file-desc (collab--make-file-desc "host-id" "my-project" "file-name.test.txt")))
    (should (equal "/host-id/my-project/file-name.test.txt"
                   (collab--encode-filename file-desc)))))

;;; Round-trip tests

(ert-deftest collab--filename-round-trip/basic ()
  "Test that parse and encode are inverse operations."
  (let* ((path "/host1/myproject/src/file.txt")
         (parsed (collab--parse-filename path))
         (encoded (collab--encode-filename parsed)))
    (should (equal path encoded))))

(ert-deftest collab--filename-round-trip/project-root ()
  "Test round-trip for project root."
  (let* ((path "/host1/myproject")
         (parsed (collab--parse-filename path))
         (encoded (collab--encode-filename parsed)))
    (should (equal path encoded))))

(ert-deftest collab--filename-round-trip/various-paths ()
  "Test round-trip for various paths."
  (dolist (path '("/host1/project/file.txt"
                  "/host1/project"
                  "/host1/_buffers/buffer123"
                  "/host1/project/src/lib/utils/helper.c"
                  "/host-id/my-project/file name with spaces.txt"))
    (let* ((parsed (collab--parse-filename path))
           (encoded (collab--encode-filename parsed)))
      (should (equal path encoded)))))

;;; Integration tests

(ert-deftest collab--file-desc-parent-encode ()
  "Test that parent and encode work together correctly."
  (let* ((file-desc (collab--make-file-desc "host1" "myproject" "src/lib/file.c"))
         (parent (collab--file-desc-parent file-desc))
         (parent-path (collab--encode-filename parent)))
    (should (equal "/host1/myproject/src/lib" parent-path))))

(ert-deftest collab--parse-parent-encode ()
  "Test parsing, getting parent, and encoding."
  (let* ((path "/host1/myproject/src/lib/file.c")
         (file-desc (collab--parse-filename path))
         (parent (collab--file-desc-parent file-desc))
         (parent-path (collab--encode-filename parent)))
    (should (equal "/host1/myproject/src/lib" parent-path))))

(ert-deftest collab--parent-chain ()
  "Test walking up the parent chain."
  (let* ((file-desc (collab--make-file-desc "host1" "myproject" "a/b/c/d/file.txt"))
         (parent1 (collab--file-desc-parent file-desc))
         (parent2 (collab--file-desc-parent parent1))
         (parent3 (collab--file-desc-parent parent2))
         (parent4 (collab--file-desc-parent parent3))
         (parent5 (collab--file-desc-parent parent4))
         (parent6 (collab--file-desc-parent parent5)))
    (should (equal (collab--encode-filename parent1) "/host1/myproject/a/b/c/d"))
    (should (equal (collab--encode-filename parent2) "/host1/myproject/a/b/c"))
    (should (equal (collab--encode-filename parent3) "/host1/myproject/a/b"))
    (should (equal (collab--encode-filename parent4) "/host1/myproject/a"))
    (should (equal (collab--encode-filename parent5) "/host1/myproject"))
    (should (equal parent6 nil))))

;;; Tests for collab--push-with-limit

(ert-deftest collab--push-with-limit/basic ()
  "Test basic push and truncation."
  (let ((lst '(3 2 1)))
    (collab--push-with-limit 4 lst 5)
    (should (equal '(4 3 2 1) lst)))
  (let ((lst '(3 2 1)))
    (collab--push-with-limit 4 lst 3)
    (should (equal '(4 3 2) lst))))

(ert-deftest collab--push-with-limit/edge-cases ()
  "Test edge cases: empty list, limit 1, limit 0."
  (let ((lst nil))
    (collab--push-with-limit 1 lst 3)
    (should (equal '(1) lst)))
  (let ((lst '(2 1)))
    (collab--push-with-limit 3 lst 1)
    (should (equal '(3) lst)))
  (let ((lst '(2 1)))
    (collab--push-with-limit 3 lst 0)
    (should (equal nil lst)))))

(provide 'collab-mode-tests)
;;; collab-mode-tests.el ends here