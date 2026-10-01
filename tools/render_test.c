/* Хостовый прогон HTML->текст рендера браузера.
 *
 *   tools/render_test.sh <body.html>
 *
 * Компилирует main.c как часть теста (переименовывая _start), кладёт тело в
 * rxbuf и вызывает build_page, печатая полученные строки и ссылки.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define _start web_guest_start
#include "../programs/web/main.c"

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s <body.html>\n", argv[0]);
        return 2;
    }
    FILE *f = fopen(argv[1], "rb");
    if (!f) {
        perror("open");
        return 1;
    }
    size_t n = fread(rxbuf, 1, RX_CAP, f);
    fclose(f);
    rx_total = (int)n;
    str_copy(cur_url, "http://test.invalid/", sizeof(cur_url));
    build_page(0);

    for (int i = 0; i < nlines; i++)
        printf("%3d |%s|\n", i, lines[i]);
    printf("--- nlines=%d nlinks=%d ---\n", nlines, nlinks);
    for (int i = 0; i < nlinks; i++)
        printf("link[%d] line=%d col=%d %s\n", i, links[i].line,
               links[i].col_mark, links[i].href);
    return 0;
}
