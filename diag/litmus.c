// RISC-V acquire/release litmus test (message-passing).
//
// Purpose: decide whether the runner's silicon honors RVWMO acquire/release to
// the strength the C++11 model (and llvm-libc's rwlock) assumes. It mirrors the
// rwlock's unlock->lock handoff exactly:
//
//   releaser (unlock): write protected data, then a RELEASE rmw on the flag
//                      -> riscv: plain store ; amoadd.w.rl
//   acquirer (lock):   ACQUIRE cas on the flag, then read protected data
//                      -> riscv: lr.w.aq/sc.w ; load
//
// If .aq/.rl are honored, an acquirer that observes flag>=1 MUST see data==MAGIC.
// A single "flag set but data stale" observation is a hardware memory-model
// violation, independent of the lock, the kernel, or the futex.
//
// Build (natively on the riscv runner): cc -O2 -pthread -o litmus litmus.c
// Run:   ./litmus [passes]

#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/sysinfo.h>
#include <time.h>

#define N (1 << 16) // independent cells per pass (each used once -> no reuse race)
#define MAGIC 0x5a5a5a5a

// data and flag live on separate cache lines to maximize the reorder window.
static struct {
  _Alignas(64) int data;
  _Alignas(64) int flag;
} cells[N];

static long passes;
static pthread_barrier_t bar;
static long long observations = 0;
static long long violations = 0;

static void pin(int cpu) {
  cpu_set_t set;
  CPU_ZERO(&set);
  CPU_SET(cpu, &set);
  if (sched_setaffinity(0, sizeof(set), &set) != 0)
    fprintf(stderr, "warning: could not pin to cpu %d\n", cpu);
}

static void *releaser(void *arg) {
  pin((int)(intptr_t)arg);
  for (long p = 0; p < passes; p++) {
    for (int i = 0; i < N; i++) { // reset
      cells[i].data = 0;
      __atomic_store_n(&cells[i].flag, 0, __ATOMIC_RELAXED);
    }
    pthread_barrier_wait(&bar); // begin run phase
    for (int i = 0; i < N; i++) {
      cells[i].data = MAGIC;                                   // protected data
      __atomic_fetch_add(&cells[i].flag, 1, __ATOMIC_RELEASE); // amoadd.w.rl
    }
    pthread_barrier_wait(&bar); // end run phase
  }
  return NULL;
}

static void *acquirer(void *arg) {
  pin((int)(intptr_t)arg);
  for (long p = 0; p < passes; p++) {
    pthread_barrier_wait(&bar); // begin run phase
    for (int i = 0; i < N; i++) {
      for (;;) { // spin until we acquire-observe flag == 1
        int expected = 1;
        if (__atomic_compare_exchange_n(&cells[i].flag, &expected, 2, 0,
                                        __ATOMIC_ACQUIRE, __ATOMIC_RELAXED))
          break; // lr.w.aq/sc.w
      }
      int d = cells[i].data; // must be MAGIC if acquire is honored
      observations++;
      if (d != MAGIC)
        violations++;
    }
    pthread_barrier_wait(&bar); // end run phase
  }
  return NULL;
}

int main(int argc, char **argv) {
  passes = (argc > 1) ? atol(argv[1]) : 50000;
  int ncpu = get_nprocs();
  int rel_cpu = 0, acq_cpu = (ncpu > 1) ? ncpu - 1 : 0;
  printf("litmus: passes=%ld cells/pass=%d ncpu=%d (releaser cpu %d, acquirer "
         "cpu %d)\n",
         passes, N, ncpu, rel_cpu, acq_cpu);
  fflush(stdout);

  pthread_barrier_init(&bar, NULL, 2);
  pthread_t rel, acq;
  pthread_create(&rel, NULL, releaser, (void *)(intptr_t)rel_cpu);
  pthread_create(&acq, NULL, acquirer, (void *)(intptr_t)acq_cpu);

  struct timespec t0, t1;
  clock_gettime(CLOCK_MONOTONIC, &t0);
  pthread_join(rel, NULL);
  pthread_join(acq, NULL);
  clock_gettime(CLOCK_MONOTONIC, &t1);
  double secs = (t1.tv_sec - t0.tv_sec) + (t1.tv_nsec - t0.tv_nsec) / 1e9;

  printf("observations=%lld violations=%lld elapsed=%.1fs (%.0f Mobs/s)\n",
         observations, violations, secs, observations / 1e6 / secs);
  if (violations) {
    printf("RESULT: FAIL - hardware does NOT honor acquire/release (RVWMO "
           "violation)\n");
    return 1;
  }
  printf("RESULT: PASS - acquire/release honored across these cores\n");
  return 0;
}
