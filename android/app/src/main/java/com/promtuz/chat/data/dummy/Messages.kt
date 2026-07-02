package com.promtuz.chat.data.dummy

import java.util.Calendar
import kotlin.uuid.ExperimentalUuidApi

@OptIn(ExperimentalUuidApi::class)
fun randId() = kotlin.uuid.Uuid.random().toHexString()

@OptIn(ExperimentalUuidApi::class)
private fun relativeTime(relTime: Long) = Calendar.getInstance().timeInMillis + relTime

data class DummyMessage(
    val id: String,
    val content: String,
    val isSent: Boolean,
    val timestamp: Long
)

/**
 * ORDER: New on top
 */
val dummyMessages = listOf(
    DummyMessage(randId(), "Hi!", true, relativeTime(-1000 * 60 * 3)),
    DummyMessage(randId(), "Sounds good! See you then 👋", false, relativeTime(-1000 * 60 * 5)),
    DummyMessage(randId(), "Perfect! Let's meet at 3 PM", true, relativeTime(-1000 * 60 * 8)),
    DummyMessage(randId(), "How about tomorrow?", false, relativeTime(-1000 * 60 * 10)),
    DummyMessage(randId(), "Sure, when are you free?", true, relativeTime(-1000 * 60 * 15)),
    DummyMessage(randId(), "Would you like to grab coffee sometime?", false, relativeTime(-1000 * 60 * 20)),
    DummyMessage(randId(), "That makes sense. Thanks for explaining!", true, relativeTime(-1000 * 60 * 25)),
    DummyMessage(randId(), "It's a combination of factors including architecture, training data, and optimization techniques", false, relativeTime(-1000 * 60 * 30)),
    DummyMessage(randId(), "How does machine learning actually work?", true, relativeTime(-1000 * 60 * 35)),
    DummyMessage(randId(), "Even tho i saw some bootcamps on youtube, i cant get it right", true, relativeTime(-1000 * 60 * 38)),
    DummyMessage(randId(), "Crazy", true, relativeTime(-1000 * 60 * 40)),
    DummyMessage(randId(), "That's awesome! Congratulations! 🎉", false, relativeTime(-1000 * 60 * 45)),
    DummyMessage(randId(), "Just got promoted at work!", true, relativeTime(-1000 * 60 * 50)),
    DummyMessage(randId(), "Not much, just the usual. You?", false, relativeTime(-1000 * 60 * 55)),
    DummyMessage(randId(), "What's up?", true, relativeTime(-1000 * 60 * 60)),
    DummyMessage(randId(), "Hey! Long time no talk", false, relativeTime(-1000 * 60 * 120)),
    DummyMessage(randId(), "Thanks! Will do", true, relativeTime(-1000 * 60 * 180)),
    DummyMessage(randId(), "Make sure to bring your laptop and charger", false, relativeTime(-1000 * 60 * 185)),
    DummyMessage(randId(), "Got it, I'll be there", true, relativeTime(-1000 * 60 * 190)),
    DummyMessage(randId(), "The meeting is scheduled for 10 AM in conference room B", false, relativeTime(-1000 * 60 * 200)),
    DummyMessage(randId(), "Can you send me the details?", true, relativeTime(-1000 * 60 * 210)),
    DummyMessage(randId(), "Don't forget about tomorrow's presentation", false, relativeTime(-1000 * 60 * 220)),
    DummyMessage(randId(), "Haha that's hilarious 😂", true, relativeTime(-1000 * 60 * 300)),
    DummyMessage(randId(), "You won't believe what happened today...", false, relativeTime(-1000 * 60 * 305)),
    DummyMessage(randId(), "Really? Tell me more", true, relativeTime(-1000 * 60 * 400)),
    DummyMessage(randId(), "I tried that new restaurant downtown", false, relativeTime(-1000 * 60 * 405)),
    DummyMessage(randId(), "How was your weekend?", true, relativeTime(-1000 * 60 * 500)),
    DummyMessage(randId(), "Good morning!", false, relativeTime(-1000 * 60 * 1440)), // 1 day ago
    DummyMessage(randId(), "Have a great evening!", true, relativeTime(-1000 * 60 * 1450)),
    DummyMessage(randId(), "Thanks for your help today", false, relativeTime(-1000 * 60 * 1460)),
    DummyMessage(randId(), "No problem, anytime!", true, relativeTime(-1000 * 60 * 2880)), // 2 days ago
    DummyMessage(randId(), "Did you finish the project?", false, relativeTime(-1000 * 60 * 2890)),
    DummyMessage(randId(), "Yeah, submitted it this morning", true, relativeTime(-1000 * 60 * 2900)),
    DummyMessage(randId(), "Great work!", false, relativeTime(-1000 * 60 * 4320)), // 3 days ago
    DummyMessage(randId(), "Hello", false, relativeTime(-1000 * 60 * 10080)) // 1 week ago
)