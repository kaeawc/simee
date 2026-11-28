// Pipe ping-pong for docs/research/tcg-context-switch.md: what one guest context switch costs.
//
//   gcc -O2 -static -o pingpong tcg-pingpong.c
//   pingpong <mode> <round trips> [working-set pages]
//
// A round trip is four pipe syscalls (write, read, read, write) and a one-byte message each way.
//   proc    two processes, pinned to one CPU: every message is a context switch with a CR3 write
//   thread  two threads of one process, pinned to one CPU: a context switch with no CR3 write
//   single  one process does the same four syscalls on its own pipes: no context switch
// With a working set, each side touches one byte on each of that many private pages between messages,
// so a flushed TLB has something to refill. Prints "<mode> <round trips> <seconds> <round trips/s>
// <user s> <sys s> <voluntary ctx> <involuntary ctx>" for the whole process tree.
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static long iters;
static size_t pages;
static int a[2], b[2]; // a: first -> second, b: second -> first

static void pin(void) {
    cpu_set_t s;
    CPU_ZERO(&s);
    CPU_SET(0, &s);
    sched_setaffinity(0, sizeof s, &s);
}

static volatile char sink;
static char *arena(void) {
    if (!pages) return NULL;
    char *p = mmap(NULL, pages * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) exit(2);
    for (size_t i = 0; i < pages; i++) p[i * 4096] = 1;
    return p;
}
static void touch(char *p) {
    for (size_t i = 0; i < pages; i++) sink += p[i * 4096];
}

static void second(void) {
    char c, *m = arena();
    for (long i = 0; i < iters; i++) {
        if (read(a[0], &c, 1) != 1) exit(3);
        touch(m);
        if (write(b[1], &c, 1) != 1) exit(3);
    }
}
static void *second_thread(void *x) {
    pin();
    second();
    return x;
}

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec * 1e-9;
}

int main(int argc, char **argv) {
    if (argc < 3) return 1;
    const char *mode = argv[1];
    iters = atol(argv[2]);
    pages = argc > 3 ? (size_t)atol(argv[3]) : 0;
    if (pipe(a) || pipe(b)) return 1;
    pin();
    char c = 'x', *m = arena();
    pthread_t th;
    pid_t pid = 0;
    int single = !strcmp(mode, "single");
    if (!strcmp(mode, "proc")) {
        pid = fork();
        if (pid == 0) {
            second();
            _exit(0);
        }
    } else if (!strcmp(mode, "thread")) {
        pthread_create(&th, NULL, second_thread, NULL);
    } else if (!single) {
        return 1;
    }
    double t0 = now();
    for (long i = 0; i < iters; i++) {
        if (write(a[1], &c, 1) != 1) return 3;
        if (single && read(a[0], &c, 1) != 1) return 3;
        if (single) touch(m);
        if (single && write(b[1], &c, 1) != 1) return 3;
        if (read(b[0], &c, 1) != 1) return 3;
        touch(m);
    }
    double dt = now() - t0;
    if (pid) waitpid(pid, NULL, 0);
    if (!pid && !single) pthread_join(th, NULL);
    struct rusage s, k;
    getrusage(RUSAGE_SELF, &s);
    getrusage(RUSAGE_CHILDREN, &k);
    printf("%s %ld %.3f %.0f %.3f %.3f %ld %ld\n", mode, iters, dt, iters / dt,
           s.ru_utime.tv_sec + s.ru_utime.tv_usec * 1e-6 + k.ru_utime.tv_sec + k.ru_utime.tv_usec * 1e-6,
           s.ru_stime.tv_sec + s.ru_stime.tv_usec * 1e-6 + k.ru_stime.tv_sec + k.ru_stime.tv_usec * 1e-6,
           s.ru_nvcsw + k.ru_nvcsw, s.ru_nivcsw + k.ru_nivcsw);
    return 0;
}
