"""Bot entry point — sets up the Discord bot, loads cogs, runs."""

from __future__ import annotations

import discord
import structlog
from discord.ext import commands

from collector.config import settings
from collector.storage.local_buffer import cleanup_uploaded, get_orphaned_sessions
from collector.utils.logging import setup_logging

log = structlog.get_logger()

# Load opus for voice receive
if not discord.opus.is_loaded():
    discord.opus.load_opus("libopus.so.0")


def create_bot() -> commands.Bot:
    intents = discord.Intents.default()
    intents.voice_states = True
    intents.guilds = True
    intents.members = True

    bot = commands.Bot(intents=intents)

    @bot.event
    async def on_ready() -> None:
        log.info("bot_ready", user=bot.user.name, guilds=len(bot.guilds))

        # Cleanup old uploaded sessions
        cleaned = cleanup_uploaded()
        if cleaned:
            log.info("cleaned_old_sessions", count=cleaned)

        # Check for orphaned sessions
        orphans = get_orphaned_sessions()
        if orphans:
            log.warning("orphaned_sessions_found", count=len(orphans), sessions=orphans)

    bot.load_extension("collector.cogs.recording")
    bot.load_extension("collector.cogs.notes")

    return bot


def main() -> None:
    setup_logging(settings.log_level)
    log.info("starting_bot")

    bot = create_bot()
    bot.run(settings.discord_token)


if __name__ == "__main__":
    main()
