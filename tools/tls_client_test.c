/* Хостовый прогон TLS-клиента браузера через обычные сокеты.
 *
 *   tools/tls_client_test.sh <host> <port> <path> [sni]
 *
 * Собирается вместе с programs/web/tls_client.c и крипто-примитивами.
 * На stdout — расшифрованное тело ответа, на stderr — диагностика.
 */

#include <errno.h>
#include <fcntl.h>
#include <netdb.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

#include "tls_client.h"

static int tr_send(void *ctx, const uint8_t *buf, size_t len) {
    int fd = *(int *)ctx;
    size_t off = 0;
    while (off < len) {
        ssize_t n = write(fd, buf + off, len - off);
        if (n < 0) {
            if (errno == EINTR)
                continue;
            return -1;
        }
        off += (size_t)n;
    }
    return (int)len;
}

static int tr_recv(void *ctx, uint8_t *buf, size_t max) {
    int fd = *(int *)ctx;
    for (;;) {
        ssize_t n = read(fd, buf, max);
        if (n < 0 && errno == EINTR)
            continue;
        return (int)n;
    }
}

static int g_urand = -1;

static void tr_random(void *ctx, uint8_t *out, size_t len) {
    (void)ctx;
    size_t off = 0;
    while (off < len) {
        ssize_t n = read(g_urand, out + off, len - off);
        if (n <= 0) {
            /* Крайне маловероятно; добиваем нулями, чтобы не зависнуть. */
            memset(out + off, 0, len - off);
            return;
        }
        off += (size_t)n;
    }
}

int main(int argc, char **argv) {
    if (argc < 4) {
        fprintf(stderr, "usage: %s <host> <port> <path> [sni]\n", argv[0]);
        return 2;
    }
    const char *host = argv[1];
    int port = atoi(argv[2]);
    const char *path = argv[3];
    const char *sni = argc > 4 ? argv[4] : host;

    g_urand = open("/dev/urandom", 0);
    if (g_urand < 0) {
        perror("open /dev/urandom");
        return 1;
    }

    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    char portstr[16];
    snprintf(portstr, sizeof(portstr), "%d", port);
    if (getaddrinfo(host, portstr, &hints, &res) != 0) {
        fprintf(stderr, "resolve %s failed\n", host);
        return 1;
    }

    int fd = socket(res->ai_family, res->ai_socktype, res->ai_protocol);
    if (fd < 0) {
        perror("socket");
        return 1;
    }
    struct timeval tv = {.tv_sec = 20, .tv_usec = 0};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv));
    if (connect(fd, res->ai_addr, res->ai_addrlen) < 0) {
        perror("connect");
        return 1;
    }
    freeaddrinfo(res);

    static tls_conn conn; /* ~50 КБ — не на стеке */
    tls_transport tr;
    tr.send = tr_send;
    tr.recv = tr_recv;
    tr.random = tr_random;
    tr.ctx = &fd;
    tls_conn_init(&conn, tr);

    if (tls_handshake(&conn, sni) < 0) {
        fprintf(stderr, "handshake failed: %s\n", tls_error(&conn));
        return 1;
    }
    fprintf(stderr, "handshake ok with %s\n", sni);

    char req[1024];
    int rl = snprintf(req, sizeof(req),
                      "GET %s HTTP/1.0\r\nHost: %s\r\n"
                      "User-Agent: VitaminOS-web/1.0\r\n"
                      "Accept: text/html\r\nConnection: close\r\n\r\n",
                      path, host);
    if (tls_write(&conn, (const uint8_t *)req, (size_t)rl) < 0) {
        fprintf(stderr, "write failed: %s\n", tls_error(&conn));
        return 1;
    }

    uint8_t buf[4096];
    size_t total = 0;
    int n;
    while ((n = tls_read(&conn, buf, sizeof(buf))) > 0) {
        fwrite(buf, 1, (size_t)n, stdout);
        total += (size_t)n;
        if (total > 500000)
            break;
    }
    if (n < 0)
        fprintf(stderr, "read error: %s\n", tls_error(&conn));
    fprintf(stderr, "\n[received %zu bytes]\n", total);
    return n < 0 ? 1 : 0;
}
