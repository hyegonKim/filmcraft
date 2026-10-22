"""Independent read-only UI probe; observes stalls during another connection's commands."""
import json
import socket
import threading
import time
from pathlib import Path


class UIProbe:
    def __init__(self, port, output, phase=lambda: 'verification', anchor=None, finished=lambda: False):
        self.port = port
        self.output = Path(output)
        self.phase = phase
        self.anchor = time.monotonic() if anchor is None else anchor
        self.finished = finished
        self.records = []
        self.stop_event = threading.Event()
        self.thread = threading.Thread(target=self._guarded_run, daemon=True)
        self.thread.start()

    def _guarded_run(self):
        try:
            self._run()
        except Exception as error:
            self.records.append({"phase": "probe", "seconds": 0, "error": repr(error)})

    def _run(self):
        stream = None
        connection = None
        with self.output.open('w') as log:
            try:
                connection = socket.create_connection(('127.0.0.1', self.port), timeout=15)
                connection.settimeout(15)
                stream = connection.makefile('rwb')
                while not self.stop_event.is_set():
                    started = time.monotonic()
                    cpu_started = time.thread_time()
                    row = {'phase': self.phase(), 'sinceStartSeconds': started - self.anchor}
                    try:
                        stream.write(b'{"method":"ui.inspect","params":{}}\n')
                        stream.flush()
                        written = time.monotonic()
                        line = stream.readline()
                        received = time.monotonic()
                        row['writeSeconds'] = written - started
                        row['replyWaitSeconds'] = received - written
                        if not line:
                            if self.finished():
                                break
                            raise ConnectionError('application closed before verification completed')
                        value = json.loads(line)
                        row['decodeSeconds'] = time.monotonic() - received
                        if not value.get('ok'):
                            raise ValueError(str(value.get('error')))
                        state = value.get('result', {}).get('input', {})
                        row['window'] = {key: state.get(key) for key in ['viewportVisible', 'viewportOccluded', 'secondsSinceUiPass']}
                    except (OSError, ValueError) as error:
                        if self.finished():
                            break
                        row['error'] = str(error)
                    row['seconds'] = time.monotonic() - started
                    row['threadCpuSeconds'] = time.thread_time() - cpu_started
                    self.records.append(row)
                    log.write(json.dumps(row) + '\n')
                    log.flush()
                    if 'error' in row:
                        break  # Preserve the failure; do not repeatedly retry a blocked connection.
                    if self.stop_event.wait(max(0, 1 - row['seconds'])):
                        break
            except OSError as error:
                row = {'phase': self.phase(), 'seconds': 0, 'error': str(error)}
                self.records.append(row)
                log.write(json.dumps(row) + '\n')
            finally:
                if stream is not None:
                    stream.close()
                if connection is not None:
                    connection.close()

    def stop(self):
        self.stop_event.set()
        self.thread.join(timeout=20)
        values = sorted(row['seconds'] for row in self.records if 'error' not in row)
        return {'requests': len(values), 'errors': sum('error' in row for row in self.records),
                'p95Seconds': values[int((len(values) - 1) * .95)] if values else None,
                'maxSeconds': max(values, default=None),
                'overOneSecond': sum(value > 1 for value in values),
                'overSixSeconds': sum(value >= 6 for value in values),
                'stopped': not self.thread.is_alive()}
