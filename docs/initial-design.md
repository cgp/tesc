A tool that records and compares statistics about processes running on servers. It can trigger load testing jobs to help evaluate performance.

### How to Use
* Specify a service configuration to grab diagnostics against. (Specify hostnames, IP groups, autoscaling groups? Docker hostnames)
* At this point you should be able to start monitoring/recording different metrics on the 
* Specify a load test

### Metrics / Information pulled
* Disk space
* CPU load
* Memory used/reserved
* Open ports/mounts

### Features and Implementation
* traces from hostname to ecs instances as well as just accepting IP addresses to SSH and directly review stats using SSH on the server. It works using real time stats directly off of the box. 

### Implementation Details
* Look and feel similar to task manager?

### Footnotes
Known gap: if the service autoscales we are probably going to miss that for now, and that would be a good thing to know. It might even be good to know what it is *supposed* to trigger on and evaluate how long it takes to trigger on that. This feels like a hard problem and I will leave it out for now.
