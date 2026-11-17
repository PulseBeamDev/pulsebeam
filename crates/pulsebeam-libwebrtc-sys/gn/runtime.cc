#include <string>

int main(int argc, char** argv) {
  const std::string name(argc > 0 ? argv[0] : "runtime");
  return name.empty();
}
