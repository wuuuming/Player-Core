/* The demux thread's lock and wakeup, and the interrupt cells read by FFmpeg. */
#ifndef BN_THREAD_H
#define BN_THREAD_H
#ifdef _WIN32
#include <windows.h>
typedef HANDLE BnThread;
typedef SRWLOCK BnLock;
typedef CONDITION_VARIABLE BnCondition;
typedef volatile LONG BnAtomic;
typedef volatile LONG64 BnAtomic64;
#define BN_THREAD_RESULT DWORD WINAPI
#define bn_start(fn, arg) CreateThread(NULL, 0, fn, arg, 0, NULL)
static inline void bn_join(BnThread t) { WaitForSingleObject(t, INFINITE); CloseHandle(t); }
static inline int bn_sync_init(BnLock *l, BnCondition *c) {
    InitializeSRWLock(l); InitializeConditionVariable(c); return 0;
}
#define bn_sync_destroy(l, c) ((void)0)
#define bn_lock AcquireSRWLockExclusive
#define bn_unlock ReleaseSRWLockExclusive
#define bn_wake WakeAllConditionVariable
#define bn_wait(c, l) SleepConditionVariableSRW(c, l, INFINITE, 0)
#define bn_wait_ms(c, l, ms) SleepConditionVariableSRW(c, l, ms, 0)
#define bn_load(p) InterlockedCompareExchange(p, 0, 0)
#define bn_load64(p) InterlockedCompareExchange64(p, 0, 0)
#define bn_store InterlockedExchange
#define bn_store64 InterlockedExchange64
#define bn_add64 InterlockedExchangeAdd64
#else
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <time.h>
typedef pthread_t BnThread;
typedef pthread_mutex_t BnLock;
typedef pthread_cond_t BnCondition;
typedef atomic_int BnAtomic;
typedef _Atomic int64_t BnAtomic64;
#define BN_THREAD_RESULT void *
static inline BnThread bn_start(void *(*fn)(void *), void *arg) {
    BnThread t;
    return pthread_create(&t, NULL, fn, arg) == 0 ? t : 0;
}
static inline void bn_join(BnThread t) { pthread_join(t, NULL); }
static inline int bn_sync_init(BnLock *l, BnCondition *c) {
    if (pthread_mutex_init(l, NULL)) return -1;
    if (!pthread_cond_init(c, NULL)) return 0;
    pthread_mutex_destroy(l); return -1;
}
static inline void bn_sync_destroy(BnLock *l, BnCondition *c) {
    pthread_cond_destroy(c); pthread_mutex_destroy(l);
}
#define bn_lock pthread_mutex_lock
#define bn_unlock pthread_mutex_unlock
#define bn_wake pthread_cond_broadcast
#define bn_wait pthread_cond_wait
static inline int bn_wait_ms(BnCondition *c, BnLock *l, int ms) {
    struct timespec end;
    clock_gettime(CLOCK_MONOTONIC, &end);
    end.tv_nsec += ms * 1000000L;
    end.tv_sec += end.tv_nsec / 1000000000L;
    end.tv_nsec %= 1000000000L;
    return pthread_cond_clockwait(c, l, CLOCK_MONOTONIC, &end) == 0;
}
#define bn_load atomic_load
#define bn_load64 atomic_load
#define bn_store atomic_store
#define bn_store64 atomic_store
#define bn_add64 atomic_fetch_add
#endif
#endif
