// Keep Gradle output under Cargo's target directory, not in the checkout.
val paseoBuildDir = providers.gradleProperty("paseoBuildDir").get()
layout.buildDirectory.set(file(paseoBuildDir))
subprojects {
    layout.buildDirectory.set(file("$paseoBuildDir/${project.name}"))
}
