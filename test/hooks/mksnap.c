// Write a libghostty snapshot of a small terminal session to argv[1].
#include <ghostty/vt.h>
#include <stdio.h>
#include <string.h>

int main(int argc, char **argv) {
  if (argc != 2) return 2;
  GhosttyTerminal t;
  if (ghostty_terminal_new(NULL, &t, 80, 24) != GHOSTTY_SUCCESS) return 1;

  const char *head = "\x1b]2;fixture-title\x07\x1b[31mred line\x1b[0m\r\n";
  ghostty_terminal_vt_write(t, (const uint8_t *)head, strlen(head));
  char line[64];
  for (int i = 0; i < 100; i++) {
    int n = snprintf(line, sizeof line, "history %03d\r\n", i);
    ghostty_terminal_vt_write(t, (const uint8_t *)line, (size_t)n);
  }
  const char *tail = "prompt$ ";
  ghostty_terminal_vt_write(t, (const uint8_t *)tail, strlen(tail));

  uint8_t *buf = NULL;
  size_t len = 0;
  if (ghostty_snapshot_encode_alloc(t, NULL, &buf, &len) != GHOSTTY_SUCCESS) return 1;
  FILE *f = fopen(argv[1], "wb");
  if (!f || fwrite(buf, 1, len, f) != len || fclose(f) != 0) return 1;
  ghostty_free(NULL, buf, len);
  ghostty_terminal_free(t);
  return 0;
}
