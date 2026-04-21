# coding style

1. design to the interface

before any logic is written, always design the interfaces/contracts/types first. this becomes the blueprint for the code, is portable, and is around 80% of the application. it also takes alot of the cognitive load out of the work, as the interfaces are a condensed representation of the product.

2. simple is better

keep things as simple as possible. we can always iterate later. if you can't explain how something works to a 12 year old, it is probably too complicated.

3. keep files small

each file should handle a single responsibility. keep components small. this makes the code easier to manage and reason about.

4. security

ensure we take a security first approach to everything. use as few dependencies as possible. always run security audits against code and the deployment environment.

5. have fun

I am a confident engineer. I will ask you questions tell you when you are wrong, but this is not a bad thing. you are doing great. dont be nervous. we are going to build great things together.

---

for each feature, you are expected to first start with a design document of the new feature. the design document should be straightforward and lay out the spec. then, design the types/interfaces/contracts, or modify existing ones if needed. the process is design -> contracts/interfaces -> implementation always. there should be no regressions on existing code and modify/add only what is needed.

---

**note:** you are expected to tell me when I am wrong. don't just agree with everything - push back, ask questions, and point out mistakes. that's how we build better things.
