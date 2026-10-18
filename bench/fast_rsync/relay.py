# TCP relay that delays every chunk by DELAY seconds in each direction (a stand-in for
# netem's delay, which this kernel lacks). Usage: relay.py LISTEN_HOST LISTEN_PORT TO_HOST TO_PORT DELAY
import asyncio, sys

LH, LP, TH, TP, DELAY = sys.argv[1], int(sys.argv[2]), sys.argv[3], int(sys.argv[4]), float(sys.argv[5])

async def pipe(reader, writer):
    loop = asyncio.get_running_loop()
    q = asyncio.Queue()
    async def send():
        while True:
            due, data = await q.get()
            wait = due - loop.time()
            if wait > 0:
                await asyncio.sleep(wait)
            if not data:
                try:
                    writer.write_eof()
                except OSError:
                    pass
                return
            writer.write(data)
            await writer.drain()
    task = asyncio.create_task(send())
    try:
        while True:
            data = await reader.read(1 << 16)
            q.put_nowait((loop.time() + DELAY, data))
            if not data:
                break
        await task
    except (ConnectionError, OSError):
        task.cancel()

async def handle(cr, cw):
    sr, sw = await asyncio.open_connection(TH, TP)
    await asyncio.gather(pipe(cr, sw), pipe(sr, cw), return_exceptions=True)
    for w in (cw, sw):
        w.close()

async def main():
    srv = await asyncio.start_server(handle, LH, LP)
    async with srv:
        await srv.serve_forever()

asyncio.run(main())
