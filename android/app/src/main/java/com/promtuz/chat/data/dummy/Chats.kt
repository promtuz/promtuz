package com.promtuz.chat.data.dummy

import com.promtuz.chat.domain.model.Chat
import com.promtuz.chat.domain.model.ChatType
import com.promtuz.chat.domain.model.LastMessage
import kotlin.random.Random

fun randKey() = Random.nextBytes(ByteArray(32))

// @formatter:off
val dummyChats = listOf(
    Chat(randKey(), "Alice Chen", LastMessage("See you tomorrow!", 1761847076578L)),
    Chat(randKey(), "Work Team", LastMessage("David: Meeting moved to 3 PM", 1761846656578L), ChatType.Group),
    Chat(randKey(), "Bob Martinez", LastMessage("Did you get the files?", 1761844676578L)),
    Chat(randKey(), "Mom", LastMessage("Call me when you're free", 1761840176578L)),
    Chat(randKey(), "Sarah Williams", LastMessage("Thanks for the help 😊", 1761836576578L)),
    Chat(randKey(), "Gym Buddies", LastMessage("Jake: Who's in for tomorrow 6 AM?", 1761829376578L), ChatType.Group),
    Chat(randKey(), "Mike Johnson", LastMessage("Running late, be there in 10", 1761778976578L)),
    Chat(randKey(), "Emma Davis", LastMessage("That sounds great!", 1761757376578L)),
    Chat(randKey(), "College Friends", LastMessage("You: Anyone up for weekend plans?", 1761674576578L), ChatType.Group),
    Chat(randKey(), "Alex Kim", LastMessage("Can we reschedule?", 1761588176578L)),
    Chat(randKey(), "Lisa Brown", LastMessage("Just finished the meeting", 1761501776578L)),
    Chat(randKey(), "Dad", LastMessage("How's work going?", 1761415376578L)),
    Chat(randKey(), "Tom Anderson", LastMessage("Check out this link", 1761328976578L)),
    Chat(randKey(), "Project Alpha", LastMessage("Nina: Deadline extended by 2 days", 1761156176578L), ChatType.Group),
    Chat(randKey(), "Nina Patel", LastMessage("Happy birthday! 🎉", 1760983376578L)),
    Chat(randKey(), "Chris Lee", LastMessage("I'll send it over now", 1760810576578L)),
    Chat(randKey(), "Maya Rodriguez", LastMessage("Perfect timing", 1760551376578L)),
    Chat(randKey(), "Apartment Neighbors", LastMessage("Sarah: Maintenance tomorrow 9-11 AM", 1760292176578L), ChatType.Group),
    Chat(randKey(), "David Smith", LastMessage("Let me know when you're free", 1760032976578L)),
    Chat(randKey(), "Sophie Turner", LastMessage("Absolutely!", 1759687376578L)),
    Chat(randKey(), "Ryan Cooper", LastMessage("Got it, thanks", 1759255376578L)),
    Chat(randKey(), "Book Club", LastMessage("Emily: Next meeting March 15th", 1758823376578L), ChatType.Group),
    Chat(randKey(), "Zara Ahmed", LastMessage("On my way", 1758391376578L)),
    Chat(randKey(), "Coffee Meetup", LastMessage("You: Same place next week?", 1757527376578L), ChatType.Group),
    Chat(randKey(), "James Wilson", LastMessage("Congrats on the promotion!", 1756663376578L)),
    Chat(randKey(), "Olivia Zhang", LastMessage("Safe travels!", 1755367376578L)),
    Chat(randKey(), "Weekend Hikers", LastMessage("Mark: Trail conditions are perfect", 1754071376578L), ChatType.Group),
    Chat(randKey(), "Rachel Green", LastMessage("Miss you! Let's catch up soon", 1751479376578L)),
)
// @formatter:on