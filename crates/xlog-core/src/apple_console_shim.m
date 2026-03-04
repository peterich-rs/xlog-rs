#import <Foundation/Foundation.h>
#import <os/log.h>
#include <stdio.h>

static const char* safe_cstr(const char* value) {
    return value == NULL ? "" : value;
}

static os_log_type_t to_oslog_type(int level) {
    switch (level) {
        case 0:  // Verbose
        case 1:  // Debug
            return OS_LOG_TYPE_DEBUG;
        case 2:  // Info
        case 3:  // Warn
            return OS_LOG_TYPE_INFO;
        case 4:  // Error
            return OS_LOG_TYPE_ERROR;
        case 5:  // Fatal
            return OS_LOG_TYPE_FAULT;
        default:
            return OS_LOG_TYPE_DEFAULT;
    }
}

void xlog_core_apple_console_printf(const char* text) {
    if (text == NULL) {
        return;
    }
    printf("%s\n", text);
}

void xlog_core_apple_console_nslog(const char* text) {
    if (text == NULL) {
        return;
    }
    @autoreleasepool {
        NSLog(@"%s", text);
    }
}

void xlog_core_apple_console_oslog(
    int level,
    const char* tag,
    const char* file,
    int line,
    const char* func,
    const char* msg
) {
    if (msg == NULL) {
        return;
    }
    @autoreleasepool {
        os_log_t log_t = os_log_create("", safe_cstr(tag));
        os_log_with_type(
            log_t,
            to_oslog_type(level),
            "[%{public}s:%d, %{public}s][%{public}s",
            safe_cstr(file),
            line,
            safe_cstr(func),
            safe_cstr(msg)
        );
    }
}
