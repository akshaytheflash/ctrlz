import asyncio, json, os
from playwright.async_api import async_playwright

COMP = os.path.abspath(os.path.join(os.path.dirname(__file__), "comp.html"))

async def main():
    async with async_playwright() as p:
        b = await p.chromium.launch()
        pg = await b.new_page(viewport={"width": 1920, "height": 1080})
        errs = []
        pg.on("pageerror", lambda e: errs.append(str(e)))
        await pg.goto("file://" + COMP)
        await pg.evaluate("window.__ready")
        ev = await pg.evaluate("window.__events()")
        json.dump(ev, open(os.path.join(os.path.dirname(__file__), "events.json"), "w"), indent=1)
        print(len(ev["events"]), "events, T", ev["T"], "errors", errs[:3])
        await b.close()

asyncio.run(main())