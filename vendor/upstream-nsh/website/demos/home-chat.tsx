// The home page's hero: the gallery's AI chat (examples/gallery, Chat),
// playing one scenario on load and staying interactive.
import { mount } from "nsh";
import { Chat } from "./hero-chat/Chat";

mount(() => <Chat autoplay="search" />);
