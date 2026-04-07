# Prompt before download and GC unused images

Moved the image-change prompt to before layer download so "keep old
environment" skips downloading entirely. When user re-creates an
environment, `gc_unused_image` scans all project digest files — if
no project references the old image, the cached image is deleted.
