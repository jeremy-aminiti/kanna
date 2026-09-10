#define _DARWIN_C_SOURCE
#define _POSIX_C_SOURCE 200809L

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#ifndef PATH_MAX
#define PATH_MAX 4096
#endif

static const char *package_name;
static const char *report_path;

static void die(const char *format, ...) {
    va_list args;
    fprintf(stderr, "%s: ", package_name);
    va_start(args, format);
    vfprintf(stderr, format, args);
    va_end(args);
    fputc('\n', stderr);
    exit(1);
}

static void path_join(char *output, size_t size, const char *left, const char *right) {
    int written = snprintf(output, size, "%s%s%s", left, left[0] ? "/" : "", right);
    if (written < 0 || (size_t)written >= size) {
        die("path is too long: %s/%s", left, right);
    }
}

static ssize_t read_link(const char *path, char *target, size_t size) {
    ssize_t length = readlink(path, target, size - 1);
    if (length < 0) {
        die("cannot read symlink %s: %s", path, strerror(errno));
    }
    if ((size_t)length >= size - 1) {
        die("symlink target is too long: %s", path);
    }
    target[length] = '\0';
    return length;
}

static size_t split_components(char *value, char **parts, size_t capacity, bool reject_escape) {
    size_t count = 0;
    char *cursor = value;
    while (*cursor) {
        while (*cursor == '/') cursor++;
        if (!*cursor) break;
        char *component = cursor;
        while (*cursor && *cursor != '/') cursor++;
        if (*cursor) *cursor++ = '\0';
        if (!strcmp(component, ".")) continue;
        if (!strcmp(component, "..")) {
            if (count == 0) {
                if (reject_escape) return (size_t)-1;
                continue;
            }
            count--;
            continue;
        }
        if (count == capacity) die("path has too many components");
        parts[count++] = component;
    }
    return count;
}

static void report(const char *format, ...) {
    FILE *file = fopen(report_path, "a");
    if (!file) die("cannot append overlay report: %s", strerror(errno));
    va_list args;
    va_start(args, format);
    vfprintf(file, format, args);
    va_end(args);
    fputc('\n', file);
    if (fclose(file)) die("cannot close overlay report: %s", strerror(errno));
}

/* Bazel contains archive-absolute links by prefixing the extraction root. */
static const char *archive_link_target(
    const char *source_root,
    const char *extracted_target
) {
    size_t root_length = strlen(source_root);
    if (!strncmp(extracted_target, source_root, root_length) &&
        extracted_target[root_length] == '/') {
        return extracted_target + root_length;
    }
    return extracted_target;
}

/*
 * Convert every target to a canonical path relative to the link's parent.
 * Debian absolute links name locations inside the future target root, so `/x`
 * becomes an in-sysroot relative link. A relative target that climbs above
 * that root is malformed and rejected rather than allowed to reach the host.
 */
static void normalize_link(const char *relative_path, const char *target, char *output, size_t size) {
    char parent_buffer[PATH_MAX];
    char target_buffer[PATH_MAX];
    char *root_parts[PATH_MAX / 2];
    char *parent_parts[PATH_MAX / 2];
    size_t root_count = 0;
    size_t parent_count;

    if (strlen(relative_path) >= sizeof(parent_buffer) || strlen(target) >= sizeof(target_buffer)) {
        die("symlink path is too long: %s", relative_path);
    }

    strcpy(parent_buffer, relative_path);
    char *slash = strrchr(parent_buffer, '/');
    if (slash) *slash = '\0'; else parent_buffer[0] = '\0';

    char parent_copy[PATH_MAX];
    strcpy(parent_copy, parent_buffer);
    parent_count = split_components(parent_copy, parent_parts, PATH_MAX / 2, true);
    if (parent_count == (size_t)-1) die("invalid exported path: %s", relative_path);

    if (target[0] == '/') {
        strcpy(target_buffer, target);
    } else {
        int written = snprintf(target_buffer, sizeof(target_buffer), "%s%s%s",
                               parent_buffer, parent_buffer[0] ? "/" : "", target);
        if (written < 0 || (size_t)written >= sizeof(target_buffer)) {
            die("symlink target is too long: %s", relative_path);
        }
    }
    root_count = split_components(target_buffer, root_parts, PATH_MAX / 2, target[0] != '/');
    if (root_count == (size_t)-1) {
        die("relative symlink escapes sysroot: %s -> %s", relative_path, target);
    }

    size_t common = 0;
    while (common < parent_count && common < root_count &&
           !strcmp(parent_parts[common], root_parts[common])) {
        common++;
    }

    size_t used = 0;
    for (size_t index = common; index < parent_count; index++) {
        int written = snprintf(output + used, size - used, "%s..", used ? "/" : "");
        if (written < 0 || (size_t)written >= size - used) die("normalized link is too long");
        used += (size_t)written;
    }
    for (size_t index = common; index < root_count; index++) {
        int written = snprintf(output + used, size - used, "%s%s", used ? "/" : "", root_parts[index]);
        if (written < 0 || (size_t)written >= size - used) die("normalized link is too long");
        used += (size_t)written;
    }
    if (used == 0) {
        if (size < 2) die("normalized link is too long");
        strcpy(output, ".");
    }
}

static bool files_equal(const char *left, const char *right) {
    int left_fd = open(left, O_RDONLY);
    int right_fd = open(right, O_RDONLY);
    if (left_fd < 0 || right_fd < 0) die("cannot compare duplicate files: %s", strerror(errno));
    char left_buffer[65536];
    char right_buffer[65536];
    bool equal = true;
    for (;;) {
        ssize_t left_size = read(left_fd, left_buffer, sizeof(left_buffer));
        ssize_t right_size = read(right_fd, right_buffer, sizeof(right_buffer));
        if (left_size < 0 || right_size < 0) die("cannot compare duplicate files: %s", strerror(errno));
        if (left_size != right_size || (left_size && memcmp(left_buffer, right_buffer, (size_t)left_size))) {
            equal = false;
            break;
        }
        if (left_size == 0) break;
    }
    close(left_fd);
    close(right_fd);
    return equal;
}

static void copy_file(const char *source, const char *destination, mode_t mode) {
    int input = open(source, O_RDONLY);
    int output = open(destination, O_WRONLY | O_CREAT | O_EXCL, mode & 07777);
    if (input < 0 || output < 0) die("cannot copy %s: %s", source, strerror(errno));
    if (fchmod(output, mode & 07777)) die("cannot set mode on %s: %s", destination, strerror(errno));
    char buffer[65536];
    for (;;) {
        ssize_t count = read(input, buffer, sizeof(buffer));
        if (count < 0) die("cannot read %s: %s", source, strerror(errno));
        if (count == 0) break;
        ssize_t offset = 0;
        while (offset < count) {
            ssize_t written = write(output, buffer + offset, (size_t)(count - offset));
            if (written < 0) die("cannot write %s: %s", destination, strerror(errno));
            offset += written;
        }
    }
    if (close(input) || close(output)) die("cannot finish copying %s: %s", destination, strerror(errno));
}

static void merge_entry(const char *source_root, const char *destination_root, const char *relative_path) {
    char source[PATH_MAX];
    char destination[PATH_MAX];
    struct stat source_stat;
    struct stat destination_stat;
    path_join(source, sizeof(source), source_root, relative_path);
    path_join(destination, sizeof(destination), destination_root, relative_path);
    if (lstat(source, &source_stat)) die("cannot inspect %s: %s", source, strerror(errno));
    bool destination_exists = lstat(destination, &destination_stat) == 0;

    if (S_ISDIR(source_stat.st_mode)) {
        if (destination_exists &&
            (!S_ISDIR(destination_stat.st_mode) ||
             (source_stat.st_mode & 07777) != (destination_stat.st_mode & 07777))) {
            die("conflicting duplicate path %s", relative_path);
        }
        if (!destination_exists && mkdir(destination, 0700)) {
            die("cannot create directory %s: %s", destination, strerror(errno));
        }
        if (chmod(destination, (source_stat.st_mode & 07777) | S_IWUSR | S_IXUSR)) {
            die("cannot make directory writable %s: %s", destination, strerror(errno));
        }
        struct dirent **entries;
        int entry_count = scandir(source, &entries, NULL, alphasort);
        if (entry_count < 0) die("cannot scan directory %s: %s", source, strerror(errno));
        for (int index = 0; index < entry_count; index++) {
            struct dirent *entry = entries[index];
            if (!strcmp(entry->d_name, ".") || !strcmp(entry->d_name, "..")) {
                free(entry);
                continue;
            }
            char child[PATH_MAX];
            path_join(child, sizeof(child), relative_path, entry->d_name);
            merge_entry(source_root, destination_root, child);
            free(entry);
        }
        free(entries);
        if (chmod(destination, source_stat.st_mode & 07777)) {
            die("cannot set mode on directory %s: %s", destination, strerror(errno));
        }
        return;
    }

    if (S_ISREG(source_stat.st_mode)) {
        if (destination_exists) {
            if (!S_ISREG(destination_stat.st_mode) ||
                (source_stat.st_mode & 07777) != (destination_stat.st_mode & 07777) ||
                source_stat.st_size != destination_stat.st_size || !files_equal(source, destination)) {
                die("conflicting duplicate path %s", relative_path);
            }
            report("identical-file %s %s", package_name, relative_path);
            return;
        }
        copy_file(source, destination, source_stat.st_mode);
        return;
    }

    if (S_ISLNK(source_stat.st_mode)) {
        char source_target[PATH_MAX];
        char normalized_source[PATH_MAX];
        read_link(source, source_target, sizeof(source_target));
        const char *logical_source_target = archive_link_target(source_root, source_target);
        normalize_link(relative_path, logical_source_target, normalized_source, sizeof(normalized_source));
        if (destination_exists) {
            char destination_target[PATH_MAX];
            char normalized_destination[PATH_MAX];
            if (!S_ISLNK(destination_stat.st_mode)) die("conflicting duplicate path %s", relative_path);
            read_link(destination, destination_target, sizeof(destination_target));
            normalize_link(relative_path, destination_target, normalized_destination, sizeof(normalized_destination));
            if (strcmp(normalized_source, normalized_destination)) {
                die("conflicting duplicate path %s", relative_path);
            }
            report("identical-symlink %s %s -> %s", package_name, relative_path, normalized_source);
            return;
        }
        if (symlink(normalized_source, destination)) {
            die("cannot create symlink %s -> %s: %s", relative_path, normalized_source, strerror(errno));
        }
        if (strcmp(logical_source_target, normalized_source)) {
            report("normalized-symlink %s %s %s -> %s", package_name, relative_path, logical_source_target, normalized_source);
        }
        return;
    }

    die("unsupported archive entry type at %s", relative_path);
}

int main(int argc, char **argv) {
    if (argc != 5) {
        fprintf(stderr, "usage: sysroot_overlay STAGING SYSROOT PACKAGE REPORT\n");
        return 2;
    }
    package_name = argv[3];
    report_path = argv[4];
    struct dirent **entries;
    int entry_count = scandir(argv[1], &entries, NULL, alphasort);
    if (entry_count < 0) die("cannot scan staged payload: %s", strerror(errno));
    for (int index = 0; index < entry_count; index++) {
        struct dirent *entry = entries[index];
        if (strcmp(entry->d_name, ".") && strcmp(entry->d_name, "..")) {
            merge_entry(argv[1], argv[2], entry->d_name);
        }
        free(entry);
    }
    free(entries);
    return 0;
}
