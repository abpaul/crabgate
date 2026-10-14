import concurrent.futures
import http.client
import sys
import time


def run(path, port, count):
    connection = http.client.HTTPConnection("localhost", port, timeout=10)
    passed = 0
    failed = 0
    sizes = set()
    for _ in range(count):
        try:
            connection.request("GET", path)
            response = connection.getresponse()
            body = response.read()
            sizes.add(len(body))
            if response.status == 200 and len(body) > 1000:
                passed += 1
            else:
                failed += 1
        except OSError:
            failed += 1
            connection.close()
            connection = http.client.HTTPConnection("localhost", port, timeout=10)
    connection.close()
    return passed, failed, sizes


def main():
    path, request_count, worker_count, port = sys.argv[1:5]
    request_count, worker_count, port = map(int, (request_count, worker_count, port))
    counts = [request_count // worker_count + (i < request_count % worker_count) for i in range(worker_count)]
    start = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=worker_count) as pool:
        results = list(pool.map(lambda count: run(path, port, count), counts))
    elapsed = time.monotonic() - start
    passed = sum(result[0] for result in results)
    failed = sum(result[1] for result in results)
    sizes = sorted(set().union(*(result[2] for result in results)))
    print(f"path={path} requests={request_count} workers={worker_count} elapsed={elapsed:.3f}s good={passed} bad={failed} bodies={sizes} rps={request_count / elapsed:.1f}")
    if failed:
        sys.exit(1)


if __name__ == "__main__":
    main()
