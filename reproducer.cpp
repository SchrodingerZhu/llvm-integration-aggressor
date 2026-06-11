// reproducer.cpp
#include <atomic>
#include <thread>
#include <vector>
#include <iostream>
#include <cassert>
#include <unistd.h>
#include <sys/syscall.h>
#include <linux/futex.h>
#include <sys/time.h>
#include <cstdlib>

#define FUTEX_SYSCALL_ID SYS_futex

long futex_wait(void *addr, int op, int val, const struct timespec *timeout) {
    return syscall(FUTEX_SYSCALL_ID, addr, op, val, timeout, NULL, 0);
}

long futex_wake(void *addr, int op, int val) {
    return syscall(FUTEX_SYSCALL_ID, addr, op, val, NULL, NULL, 0);
}

class Futex {
    std::atomic<uint32_t> val;
public:
    Futex(uint32_t v) : val(v) {}
    
    void store(uint32_t v, std::memory_order order = std::memory_order_seq_cst) {
        val.store(v, order);
    }
    
    uint32_t load(std::memory_order order = std::memory_order_seq_cst) const {
        return val.load(order);
    }
    
    bool compare_exchange_strong(uint32_t &expected, uint32_t desired,
                                 std::memory_order success, std::memory_order failure) {
        return val.compare_exchange_strong(expected, desired, success, failure);
    }
    
    uint32_t exchange(uint32_t desired, std::memory_order order = std::memory_order_seq_cst) {
        return val.exchange(desired, order);
    }
    
    uint32_t fetch_add(uint32_t v, std::memory_order order = std::memory_order_seq_cst) {
        return val.fetch_add(v, order);
    }

    void wait(uint32_t expected) {
        for (;;) {
            if (val.load(std::memory_order_relaxed) != expected)
                return;
            long ret = futex_wait(&val, FUTEX_WAIT_PRIVATE, expected, NULL);
            if (ret == 0 || ret == -EAGAIN)
                return;
        }
    }
    
    void wake_one() {
        futex_wake(&val, FUTEX_WAKE_PRIVATE, 1);
    }
    
    void wake_all() {
        futex_wake(&val, FUTEX_WAKE_PRIVATE, 2147483647);
    }
};

class RawMutex {
    Futex futex;
    static constexpr uint32_t UNLOCKED = 0;
    static constexpr uint32_t LOCKED = 1;
    static constexpr uint32_t IN_CONTENTION = 2;
    
    void spin_briefly() {
        std::this_thread::yield();
    }

    uint32_t spin(unsigned spin_count) {
        uint32_t result;
        for (;;) {
            result = futex.load(std::memory_order_relaxed);
            if (result != LOCKED || spin_count == 0)
                return result;
            spin_briefly();
            spin_count--;
        }
    }

    void lock_slow(unsigned spin_count) {
        uint32_t state = spin(spin_count);
        if (state == UNLOCKED &&
            futex.compare_exchange_strong(state, LOCKED, std::memory_order_acquire, std::memory_order_relaxed))
            return;
        for (;;) {
            if (state != IN_CONTENTION &&
                futex.exchange(IN_CONTENTION, std::memory_order_acquire) == UNLOCKED)
                return;
            futex.wait(IN_CONTENTION);
            state = spin(spin_count);
        }
    }

public:
    RawMutex() : futex(UNLOCKED) {}
    
    void lock() {
        uint32_t expected = UNLOCKED;
        if (futex.compare_exchange_strong(expected, LOCKED, std::memory_order_acquire, std::memory_order_relaxed))
            return;
        lock_slow(100);
    }
    
    void unlock() {
        uint32_t prev = futex.exchange(UNLOCKED, std::memory_order_release);
        if (prev == IN_CONTENTION) {
            futex.wake_one();
        }
    }
};

class WaitingQueue : private RawMutex {
    int pending_readers = 0;
    int pending_writers = 0;
    Futex reader_serialization{0};
    Futex writer_serialization{0};
public:
    struct Guard {
        WaitingQueue &q;
        Guard(WaitingQueue &q) : q(q) { q.lock(); }
        ~Guard() { q.unlock(); }
        
        int &pending_readers() { return q.pending_readers; }
        int &pending_writers() { return q.pending_writers; }
        Futex &reader_serial() { return q.reader_serialization; }
        Futex &writer_serial() { return q.writer_serialization; }
    };
    
    Guard acquire() { return Guard(*this); }
    void wait_reader(uint32_t serial) { reader_serialization.wait(serial); }
    void wait_writer(uint32_t serial) { writer_serialization.wait(serial); }
    void notify_readers() { reader_serialization.wake_all(); }
    void notify_writers() { writer_serialization.wake_one(); }
};

namespace rwlock {
enum class Role { Reader, Writer };
enum class LockResult { Success, Busy, Overflow, Deadlock, PermissionDenied, TimedOut };
}

using rwlock::Role;
using rwlock::LockResult;

class RwState {
    int state;
public:
    static constexpr int PENDING_READER_SHIFT = 0;
    static constexpr int PENDING_WRITER_SHIFT = 1;
    static constexpr int ACTIVE_READER_SHIFT = 2;
    static constexpr int ACTIVE_WRITER_SHIFT = 31;

    static constexpr int PENDING_READER_BIT = 1 << PENDING_READER_SHIFT;
    static constexpr int PENDING_WRITER_BIT = 1 << PENDING_WRITER_SHIFT;
    static constexpr int ACTIVE_READER_COUNT_UNIT = 1 << ACTIVE_READER_SHIFT;
    static constexpr int ACTIVE_WRITER_BIT = static_cast<int>(1u << ACTIVE_WRITER_SHIFT);
    static constexpr int PENDING_MASK = PENDING_READER_BIT | PENDING_WRITER_BIT;

    constexpr RwState(int state = 0) : state(state) {}
    constexpr operator int() const { return state; }

    constexpr bool has_active_writer() const { return state < 0; }
    constexpr bool has_active_reader() const { return state >= ACTIVE_READER_COUNT_UNIT; }
    constexpr bool has_active_owner() const { return has_active_reader() || has_active_writer(); }
    constexpr bool has_last_reader() const { return (state >> ACTIVE_READER_SHIFT) == 1; }
    constexpr bool has_pending_writer() const { return state & PENDING_WRITER_BIT; }
    constexpr bool has_pending() const { return state & PENDING_MASK; }

    constexpr RwState set_writer_bit() const { return RwState(state | ACTIVE_WRITER_BIT); }
    
    bool can_acquire_reader(int preference) const {
        if (preference == 0)
            return !has_active_writer();
        else
            return !has_active_writer() && !has_pending_writer();
    }
    
    bool can_acquire_writer() const {
        return !has_active_owner();
    }

    constexpr RwState increase_reader_count() const {
        return RwState(state + ACTIVE_READER_COUNT_UNIT);
    }
};

class RawRwLock {
    int preference;
    std::atomic<int> state;
    WaitingQueue queue;

    void notify_pending_threads() {
        int status = 0;
        {
            WaitingQueue::Guard guard = queue.acquire();
            if (guard.pending_writers() > 0) {
                guard.writer_serial().fetch_add(1, std::memory_order_release);
                status = 2;
            } else if (guard.pending_readers() > 0) {
                guard.reader_serial().fetch_add(1, std::memory_order_release);
                status = 1;
            }
        }
        if (status == 1) {
            queue.notify_readers();
        } else if (status == 2) {
            queue.notify_writers();
        }
    }

public:
    RawRwLock(int preference = 1) : preference(preference), state(0) {}

    bool has_active_writer() {
        return RwState(state.load(std::memory_order_relaxed)).has_active_writer();
    }

    bool try_read_lock() {
        int old_raw = state.load(std::memory_order_relaxed);
        for (;;) {
            RwState old(old_raw);
            if (!old.can_acquire_reader(preference))
                return false;
            RwState next = old.increase_reader_count();
            if (state.compare_exchange_weak(old_raw, next, std::memory_order_acquire, std::memory_order_relaxed))
                return true;
        }
    }

    bool try_write_lock() {
        int old_raw = state.load(std::memory_order_relaxed);
        for (;;) {
            RwState old(old_raw);
            if (!old.can_acquire_writer())
                return false;
            RwState next = old.set_writer_bit();
            if (state.compare_exchange_weak(old_raw, next, std::memory_order_acquire, std::memory_order_relaxed))
                return true;
        }
    }

    void read_lock() {
        int old_raw = state.load(std::memory_order_relaxed);
        for (;;) {
            RwState old(old_raw);
            while (old.can_acquire_reader(preference)) {
                RwState next = old.increase_reader_count();
                if (state.compare_exchange_weak(old_raw, next, std::memory_order_acquire, std::memory_order_relaxed))
                    return;
                old = RwState(old_raw);
            }
            uint32_t serial;
            {
                WaitingQueue::Guard guard = queue.acquire();
                guard.pending_readers()++;
                old_raw = state.fetch_or(RwState::PENDING_READER_BIT, std::memory_order_relaxed);
                old = RwState(old_raw | RwState::PENDING_READER_BIT);
                serial = guard.reader_serial().load(std::memory_order_relaxed);
            }
            if (!old.can_acquire_reader(preference)) {
                queue.wait_reader(serial);
            }
            {
                WaitingQueue::Guard guard = queue.acquire();
                guard.pending_readers()--;
                if (guard.pending_readers() == 0) {
                    state.fetch_and(~RwState::PENDING_READER_BIT, std::memory_order_relaxed);
                }
            }
            old_raw = state.load(std::memory_order_relaxed);
        }
    }

    void write_lock() {
        int old_raw = state.load(std::memory_order_relaxed);
        for (;;) {
            RwState old(old_raw);
            while (old.can_acquire_writer()) {
                RwState next = old.set_writer_bit();
                if (state.compare_exchange_weak(old_raw, next, std::memory_order_acquire, std::memory_order_relaxed))
                    return;
                old = RwState(old_raw);
            }
            uint32_t serial;
            {
                WaitingQueue::Guard guard = queue.acquire();
                guard.pending_writers()++;
                old_raw = state.fetch_or(RwState::PENDING_WRITER_BIT, std::memory_order_relaxed);
                old = RwState(old_raw | RwState::PENDING_WRITER_BIT);
                serial = guard.writer_serial().load(std::memory_order_relaxed);
            }
            if (!old.can_acquire_writer()) {
                queue.wait_writer(serial);
            }
            {
                WaitingQueue::Guard guard = queue.acquire();
                guard.pending_writers()--;
                if (guard.pending_writers() == 0) {
                    state.fetch_and(~RwState::PENDING_WRITER_BIT, std::memory_order_relaxed);
                }
            }
            old_raw = state.load(std::memory_order_relaxed);
        }
    }

    void unlock() {
        int old_raw = state.load(std::memory_order_relaxed);
        for (;;) {
            RwState old(old_raw);
            if (old.has_active_writer()) {
                int expected = old_raw;
                if (state.compare_exchange_weak(expected, old_raw & ~RwState::ACTIVE_WRITER_BIT, std::memory_order_release, std::memory_order_relaxed)) {
                    if (old.has_pending()) {
                        notify_pending_threads();
                    }
                    return;
                }
                old_raw = expected;
            } else if (old.has_active_reader()) {
                int expected = old_raw;
                int next = old_raw - RwState::ACTIVE_READER_COUNT_UNIT;
                if (state.compare_exchange_weak(expected, next, std::memory_order_release, std::memory_order_relaxed)) {
                    RwState next_state(next);
                    if (old.has_last_reader() && next_state.has_pending()) {
                        notify_pending_threads();
                    }
                    return;
                }
                old_raw = expected;
            } else {
                assert(false && "Unlock called on unlocked rwlock");
            }
        }
    }
};

class RwLock {
    RawRwLock raw;
    std::atomic<pid_t> writer_tid;
public:
    RwLock() : raw(1), writer_tid(0) {}

    void read_lock() {
        assert(writer_tid.load(std::memory_order_relaxed) != gettid());
        raw.read_lock();
    }

    bool try_read_lock() {
        assert(writer_tid.load(std::memory_order_relaxed) != gettid());
        return raw.try_read_lock();
    }

    void write_lock() {
        pid_t tid = gettid();
        assert(writer_tid.load(std::memory_order_relaxed) != tid);
        raw.write_lock();
        writer_tid.store(tid, std::memory_order_relaxed);
    }

    bool try_write_lock() {
        pid_t tid = gettid();
        assert(writer_tid.load(std::memory_order_relaxed) != tid);
        if (raw.try_write_lock()) {
            writer_tid.store(tid, std::memory_order_relaxed);
            return true;
        }
        return false;
    }

    void unlock() {
        if (raw.has_active_writer()) {
            assert(writer_tid.load(std::memory_order_relaxed) == gettid());
            writer_tid.store(0, std::memory_order_relaxed);
        }
        raw.unlock();
    }
    
    pid_t gettid() {
        return syscall(SYS_gettid);
    }
};

struct SharedData {
    RwLock lock;
    int data = 0;
    std::atomic<int> reader_count{0};
    bool writer_flag = false;
    std::atomic<int> total_writer_count{0};
};

SharedData g_data;

void write_ops() {
    assert(!g_data.writer_flag);
    g_data.writer_flag = true;
    std::this_thread::sleep_for(std::chrono::microseconds(10));
    assert(g_data.writer_flag);
    assert(g_data.reader_count.load() == 0);
    g_data.writer_flag = false;
}

void read_ops() {
    g_data.reader_count.fetch_add(1);
    assert(!g_data.writer_flag);
    std::this_thread::sleep_for(std::chrono::microseconds(10));
    assert(!g_data.writer_flag);
    g_data.reader_count.fetch_sub(1);
}

void thread_func(int id) {
    unsigned int seed = id + time(NULL);
    for (int i = 0; i < 10000; ++i) {
        int op = rand_r(&seed) % 4;
        if (op == 0) {
            g_data.lock.read_lock();
            read_ops();
            g_data.lock.unlock();
        } else if (op == 1) {
            g_data.lock.write_lock();
            write_ops();
            g_data.total_writer_count.fetch_add(1);
            g_data.lock.unlock();
        } else if (op == 2) {
            if (g_data.lock.try_read_lock()) {
                read_ops();
                g_data.lock.unlock();
            }
        } else {
            if (g_data.lock.try_write_lock()) {
                write_ops();
                g_data.total_writer_count.fetch_add(1);
                g_data.lock.unlock();
            }
        }
    }
}

int main() {
    std::cout << "Starting stress test..." << std::endl;
    
    constexpr int NUM_THREADS = 16;
    std::vector<std::thread> threads;
    for (int i = 0; i < NUM_THREADS; ++i) {
        threads.emplace_back(thread_func, i);
    }
    
    for (auto &t : threads) {
        t.join();
    }
    
    std::cout << "Stress test finished successfully. Total write ops: " << g_data.total_writer_count.load() << std::endl;
    return 0;
}
